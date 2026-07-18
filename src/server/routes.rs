//! HTTP handlers. Reads render the read model; every mutation is five lines: parse the form, build an [`Op`], apply it with
//! the caller's board version, answer `204` — the file watcher and SSE close the loop, so the *one* board-rendering path
//! (`GET /ui/board`) serves initial load, post-action refreshes, and live updates identically.
//!
//! Errors become `DaisyUI` toasts: the response retargets itself at `#toasts` (`HX-Retarget`), and a version conflict
//! additionally asks the browser for an immediate corrective refetch (`HX-Trigger`).

use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header::HeaderName},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use super::{AppState, security::VERSION_HEADER, views};
use crate::{
    ops::{self, Op, TicketPatch},
    pr,
    store::{
        Store, StoreError,
        derive,
        model::{ColumnId, EpicId, Status, TicketId},
    },
};

// ---- error mapping ------------------------------------------------------------------------------------------------

/// A handler failure, rendered as a toast. `refresh` asks the browser to refetch the board immediately (the stale-version
/// case: the user acted on a board that no longer exists).
#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    message: String,
    refresh: bool,
}

impl AppError {
    fn not_found(what: &str) -> AppError {
        AppError { status: StatusCode::NOT_FOUND, message: format!("{what} not found"), refresh: false }
    }

    fn bad_request(message: impl Into<String>) -> AppError {
        AppError { status: StatusCode::UNPROCESSABLE_ENTITY, message: message.into(), refresh: false }
    }
}

impl From<ops::OpError> for AppError {
    fn from(e: ops::OpError) -> AppError {
        use ops::OpError;
        if e.version_conflict().is_some() {
            return AppError { status: StatusCode::CONFLICT, message: "Board changed under you — try again".into(), refresh: true };
        }
        let status = match &e {
            OpError::NotFound(_) => StatusCode::NOT_FOUND,
            OpError::AlreadyClaimed { .. } => StatusCode::CONFLICT,
            OpError::Invalid(_) | OpError::External(_) => StatusCode::UNPROCESSABLE_ENTITY,
            OpError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        AppError { status, message: e.to_string(), refresh: false }
    }
}

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> AppError {
        AppError { status: StatusCode::INTERNAL_SERVER_ERROR, message: e.to_string(), refresh: false }
    }
}

impl From<askama::Error> for AppError {
    fn from(e: askama::Error) -> AppError {
        AppError { status: StatusCode::INTERNAL_SERVER_ERROR, message: format!("template error: {e}"), refresh: false }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::warn!(status = %self.status, reason = %self.message, "request refused — rendering toast");
        let toast = views::ToastTpl::error(self.message).render().unwrap_or_default();
        let mut headers = HeaderMap::new();
        headers.insert(HeaderName::from_static("hx-retarget"), "#toasts".parse().expect("static"));
        headers.insert(HeaderName::from_static("hx-reswap"), "beforeend".parse().expect("static"));
        if self.refresh {
            headers.insert(HeaderName::from_static("hx-trigger"), "kanban:refresh-now".parse().expect("static"));
        }
        (self.status, headers, Html(toast)).into_response()
    }
}

// ---- shared plumbing ----------------------------------------------------------------------------------------------

/// Run a store-touching closure off the async threads (the advisory lock and file IO block).
async fn blocking<T: Send + 'static>(
    app: &AppState,
    f: impl FnOnce(&Store) -> Result<T, AppError> + Send + 'static,
) -> Result<T, AppError> {
    let store = app.store.clone();
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|e| AppError { status: StatusCode::INTERNAL_SERVER_ERROR, message: format!("task failed: {e}"), refresh: false })?
}

/// The board version the client was looking at, from the header glue.js stamps on every mutation.
fn client_version(headers: &HeaderMap) -> Result<u64, AppError> {
    headers
        .get(VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| AppError::bad_request("missing or malformed X-Board-Version header"))
}

fn parse_status(s: &str) -> Result<Status, AppError> {
    s.parse().map_err(AppError::bad_request)
}

fn parse_column(s: &str) -> Result<ColumnId, AppError> {
    s.parse().map_err(AppError::bad_request)
}

/// Split a comma-separated form field into trimmed, non-empty entries.
fn csv(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect()
}

/// `""` (an unselected `<select>`) means none.
fn opt(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// Apply an op with the client's board version and answer 204 — the watcher-driven SSE refresh renders the outcome.
async fn mutate(app: &AppState, headers: &HeaderMap, op: Op) -> Result<StatusCode, AppError> {
    let version = client_version(headers)?;
    blocking(app, move |store| ops::apply(store, Some(version), op).map_err(AppError::from)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Apply an op, then answer with the refreshed detail pane for `id` — for actions taken *inside* the pane (status, notes,
/// edits), so the pane the user is looking at never goes stale.
async fn mutate_then_detail(app: &AppState, headers: &HeaderMap, op: Op, id: TicketId) -> Result<Html<String>, AppError> {
    let version = client_version(headers)?;
    blocking(app, move |store| {
        ops::apply(store, Some(version), op)?;
        rendered_detail(store, &id)
    })
    .await
}

/// Render the ticket's detail pane from fresh state, computing Create PR eligibility live (see [`pr::eligible`]).
fn rendered_detail(store: &Store, id: &TicketId) -> Result<Html<String>, AppError> {
    let board = store.read_board()?;
    let claims = store.read_claims()?;
    let can_pr = board.ticket(id).is_some_and(|t| pr::eligible(store, t));
    let tpl = views::detail(&board, &claims, id, can_pr).ok_or_else(|| AppError::not_found(&id.to_string()))?;
    Ok(Html(tpl.render()?))
}

// ---- pages and fragments (read-only — these bypass ops entirely) ---------------------------------------------------

pub async fn page(State(app): State<AppState>) -> Result<Html<String>, AppError> {
    let title = app.title.clone();
    blocking(&app, move |store| Ok(Html(views::page(title, &store.read_board()?).render()?))).await
}

pub async fn board(State(app): State<AppState>, Query(filters): Query<views::Filters>) -> Result<Html<String>, AppError> {
    blocking(&app, move |store| {
        let view = derive::board_view(&store.read_board()?, &store.read_claims()?);
        // One subprocess per render, in the main checkout (the store's parent — the same derivation `ui_owner` uses).
        // Merged-ness anchors to the configured/detected main branch, so the badge means "landed in main" even when the
        // checkout sits elsewhere; no answer falls back to HEAD, and `None` degrades to flagging nothing merged.
        let unmerged = store.dir().parent().and_then(|repo| {
            let anchor = crate::config::Config::load(store.dir()).ok().and_then(|c| c.main_branch(repo));
            crate::git::unmerged_branches(repo, anchor.as_deref().unwrap_or("HEAD"))
        });
        Ok(Html(views::board(&view, &filters, unmerged.as_ref()).render()?))
    })
    .await
}

pub async fn ticket_detail(State(app): State<AppState>, Path(id): Path<String>) -> Result<Html<String>, AppError> {
    blocking(&app, move |store| rendered_detail(store, &TicketId(id))).await
}

pub async fn ticket_edit(State(app): State<AppState>, Path(id): Path<String>) -> Result<Html<String>, AppError> {
    blocking(&app, move |store| {
        let tpl = views::detail_edit(&store.read_board()?, &TicketId(id.clone())).ok_or_else(|| AppError::not_found(&id))?;
        Ok(Html(tpl.render()?))
    })
    .await
}

pub async fn epic_detail(State(app): State<AppState>, Path(id): Path<String>) -> Result<Html<String>, AppError> {
    blocking(&app, move |store| {
        let tpl = views::epic_detail(&store.read_board()?, &EpicId(id.clone())).ok_or_else(|| AppError::not_found(&id))?;
        Ok(Html(tpl.render()?))
    })
    .await
}

pub async fn epic_edit(State(app): State<AppState>, Path(id): Path<String>) -> Result<Html<String>, AppError> {
    blocking(&app, move |store| {
        let tpl = views::epic_edit(&store.read_board()?, &EpicId(id.clone())).ok_or_else(|| AppError::not_found(&id))?;
        Ok(Html(tpl.render()?))
    })
    .await
}

/// Raw markdown for the client-side renderer (and for `curl`).
pub async fn raw_ticket(State(app): State<AppState>, Path(id): Path<String>) -> Result<Response, AppError> {
    blocking(&app, move |store| {
        let board = store.read_board()?;
        let ticket = board.ticket(&TicketId(id.clone())).ok_or_else(|| AppError::not_found(&id))?;
        Ok(([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], ticket.body.clone()).into_response())
    })
    .await
}

pub async fn raw_epic(State(app): State<AppState>, Path(id): Path<String>) -> Result<Response, AppError> {
    blocking(&app, move |store| {
        let board = store.read_board()?;
        let epic = board.epic(&EpicId(id.clone())).ok_or_else(|| AppError::not_found(&id))?;
        Ok(([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], epic.body.clone()).into_response())
    })
    .await
}

// ---- mutations ------------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateTicketForm {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    epic: String,
    #[serde(default)]
    labels: String,
    #[serde(default)]
    depends_on: String,
    #[serde(default = "default_create_status")]
    status: String,
}

fn default_create_status() -> String {
    "draft".into()
}

pub async fn create_ticket(
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateTicketForm>,
) -> Result<StatusCode, AppError> {
    let op = Op::CreateTicket {
        title: form.title,
        body: form.body,
        epic: opt(form.epic).map(EpicId),
        labels: csv(&form.labels),
        depends_on: csv(&form.depends_on).into_iter().map(TicketId).collect(),
        status: parse_status(&form.status)?,
    };
    mutate(&app, &headers, op).await
}

#[derive(Debug, Deserialize)]
pub struct UpdateTicketForm {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    epic: String,
    #[serde(default)]
    labels: String,
    #[serde(default)]
    depends_on: String,
}

/// The edit form posts every field, so the patch sets every descriptive field — an emptied input really does clear it.
pub async fn update_ticket(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<UpdateTicketForm>,
) -> Result<Html<String>, AppError> {
    let id = TicketId(id);
    let patch = TicketPatch {
        title: Some(form.title),
        body: Some(form.body),
        labels: Some(csv(&form.labels)),
        depends_on: Some(csv(&form.depends_on).into_iter().map(TicketId).collect()),
        epic: Some(opt(form.epic).map(EpicId)),
    };
    mutate_then_detail(&app, &headers, Op::UpdateTicket { id: id.clone(), patch }, id).await
}

#[derive(Debug, Deserialize)]
pub struct MoveForm {
    to: String,
    #[serde(default)]
    position: Option<usize>,
}

/// A drag's drop. The owner for cards dragged into `doing` is the human at the browser (resolved once at startup from
/// `git config user.name`).
pub async fn move_ticket(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<MoveForm>,
) -> Result<StatusCode, AppError> {
    let op = Op::MoveTicket {
        id: TicketId(id),
        to: parse_column(&form.to)?,
        position: form.position,
        owner: Some(app.ui_owner.clone()),
    };
    mutate(&app, &headers, op).await
}

#[derive(Debug, Deserialize)]
pub struct StatusForm {
    status: String,
}

pub async fn ticket_status(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<StatusForm>,
) -> Result<Html<String>, AppError> {
    let id = TicketId(id);
    let op = Op::SetTicketStatus { id: id.clone(), status: parse_status(&form.status)? };
    mutate_then_detail(&app, &headers, op, id).await
}

#[derive(Debug, Deserialize)]
pub struct NoteForm {
    text: String,
}

pub async fn add_note(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<NoteForm>,
) -> Result<Html<String>, AppError> {
    let id = TicketId(id);
    let op = Op::AddNote { id: id.clone(), text: form.text, author: Some(app.ui_owner.clone()) };
    mutate_then_detail(&app, &headers, op, id).await
}

/// The Create PR button: push the done ticket's branch and open a GitHub PR — the binary's one network egress, behind
/// this explicit click. The URL lands as a progress note with `expected_version: None`: a server-derived outcome, the
/// same documented exception as `Op::StampWorktree` (a slow git action must not be voided by a board that moved
/// underneath it; glue.js still stamps `X-Board-Version` on the POST, which is all the CSRF guard needs).
pub async fn create_pr(State(app): State<AppState>, Path(id): Path<String>) -> Result<Html<String>, AppError> {
    let id = TicketId(id);
    let author = app.ui_owner.clone();
    blocking(&app, move |store| {
        let report = pr::create_pr(store, &id).map_err(|e| AppError::bad_request(format!("{e:#}")))?;
        let text = if report.created { format!("PR created: {}", report.url) } else { format!("PR already open: {}", report.url) };
        ops::apply(store, None, Op::AddNote { id: id.clone(), text, author: Some(author) })?;
        rendered_detail(store, &id)
    })
    .await
}

pub async fn delete_ticket(State(app): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Result<StatusCode, AppError> {
    mutate(&app, &headers, Op::DeleteTicket { id: TicketId(id) }).await
}

#[derive(Debug, Deserialize)]
pub struct CreateEpicForm {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    color: String,
    #[serde(default = "default_create_status")]
    status: String,
}

pub async fn create_epic(State(app): State<AppState>, headers: HeaderMap, Form(form): Form<CreateEpicForm>) -> Result<StatusCode, AppError> {
    let op = Op::CreateEpic { title: form.title, color: opt(form.color), body: form.body, status: parse_status(&form.status)? };
    mutate(&app, &headers, op).await
}

#[derive(Debug, Deserialize)]
pub struct UpdateEpicForm {
    title: String,
    #[serde(default)]
    color: String,
    #[serde(default)]
    body: String,
}

pub async fn update_epic(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<UpdateEpicForm>,
) -> Result<Html<String>, AppError> {
    let id = EpicId(id);
    let patch = crate::ops::EpicPatch { title: Some(form.title), color: opt(form.color), body: Some(form.body) };
    let op = Op::UpdateEpic { id: id.clone(), patch };
    let version = client_version(&headers)?;
    blocking(&app, move |store| {
        ops::apply(store, Some(version), op)?;
        let tpl = views::epic_detail(&store.read_board()?, &id).ok_or_else(|| AppError::not_found(&id.to_string()))?;
        Ok(Html(tpl.render()?))
    })
    .await
}

pub async fn epic_status(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<StatusForm>,
) -> Result<Html<String>, AppError> {
    let id = EpicId(id);
    let op = Op::SetEpicStatus { id: id.clone(), status: parse_status(&form.status)? };
    let version = client_version(&headers)?;
    blocking(&app, move |store| {
        ops::apply(store, Some(version), op)?;
        let tpl = views::epic_detail(&store.read_board()?, &id).ok_or_else(|| AppError::not_found(&id.to_string()))?;
        Ok(Html(tpl.render()?))
    })
    .await
}

pub async fn delete_epic(State(app): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Result<StatusCode, AppError> {
    mutate(&app, &headers, Op::DeleteEpic { id: EpicId(id) }).await
}
