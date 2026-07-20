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
        model::{ColumnId, Effort, EpicId, Status, TicketId},
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

/// An empty effort `<select>` means "inherit the session's"; anything else must name a level.
fn parse_effort(s: &str) -> Result<Option<Effort>, AppError> {
    match s.trim() {
        "" => Ok(None),
        s => s.parse().map(Some).map_err(AppError::bad_request),
    }
}

/// The model box is free text (an alias or a full id), so it only gets trimmed — blank means "inherit".
fn parse_model(s: &str) -> Option<String> {
    opt(s.trim().to_owned())
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
        // The local branch list feeds the review column's "branch gone" flag: no answer flags nothing.
        let heads = store.dir().parent().and_then(crate::git::local_heads);
        Ok(Html(views::board(&view, &filters, heads.as_ref()).render()?))
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
    #[serde(default)]
    model: String,
    #[serde(default)]
    effort: String,
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
        model: parse_model(&form.model),
        effort: parse_effort(&form.effort)?,
        auto_merge: false,
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
    #[serde(default)]
    model: String,
    #[serde(default)]
    effort: String,
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
        model: Some(parse_model(&form.model)),
        effort: Some(parse_effort(&form.effort)?),
        // No control on the edit form yet, and this is the one field an omission must not clear.
        auto_merge: None,
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
        branch: None,
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

/// The ticket's *effective* auto-merge — its own flag or its epic's. Read just before the toggle so the button always
/// flips what the pane showed; a board that moved in between is caught by the version check on the apply that follows.
async fn current_auto_merge(app: &AppState, id: &TicketId) -> Result<bool, AppError> {
    let id = id.clone();
    blocking(app, move |store| {
        let board = store.read_board()?;
        let ticket = board.ticket(&id).ok_or_else(|| AppError::not_found(&id.to_string()))?;
        Ok(derive::auto_merge(ticket, &board))
    })
    .await
}

/// The auto-merge toggle: its own route and its own confirm, deliberately not a checkbox on the edit form — that form
/// has one blanket Save, so a confirm there would fire on every unrelated edit. Only the ticket's own flag is written,
/// so a click on an epic-granted ticket clears nothing; the confirm text says as much before it fires.
pub async fn ticket_auto_merge(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let id = TicketId(id);
    let on = current_auto_merge(&app, &id).await?;
    let patch = TicketPatch { auto_merge: Some(!on), ..TicketPatch::default() };
    mutate_then_detail(&app, &headers, Op::UpdateTicket { id: id.clone(), patch }, id).await
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

pub async fn settings(State(app): State<AppState>) -> Result<Html<String>, AppError> {
    blocking(&app, |store| Ok(Html(views::settings(&crate::config::Config::load(store.dir())?, false).render()?))).await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SettingsForm {
    worktree_root: String,
    copy_to_worktrees: String,
    max_workers: String,
    idle_time: String,
    port: String,
    main_branch: String,
    poll_interval: String,
}

impl SettingsForm {
    /// Empty fields mean "unset"; numeric fields that fail to parse are a 422 toast, and the file stays untouched.
    fn into_config(self) -> Result<crate::config::Config, AppError> {
        fn num<T: std::str::FromStr>(raw: &str, field: &str) -> Result<Option<T>, AppError> {
            let raw = raw.trim();
            if raw.is_empty() {
                return Ok(None);
            }
            raw.parse().map(Some).map_err(|_| AppError::bad_request(format!("{field} must be a whole number (or empty)")))
        }
        Ok(crate::config::Config {
            worktree_root: Some(self.worktree_root.trim()).filter(|s| !s.is_empty()).map(Into::into),
            copy_to_worktrees: self.copy_to_worktrees.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect(),
            max_workers: num(&self.max_workers, "max_workers")?,
            idle_time: num(&self.idle_time, "idle_time")?,
            port: num(&self.port, "port")?,
            main_branch: Some(self.main_branch.trim()).filter(|s| !s.is_empty()).map(str::to_owned),
            poll_interval: num(&self.poll_interval, "poll_interval")?,
        })
    }
}

/// Save the settings form over `.kanban/config.json` (whole-file: the form carries every key). The poller and the
/// worktree/work-loop dials re-read per use, so everything but `port` applies live — the pane says so.
pub async fn save_settings(State(app): State<AppState>, Form(form): Form<SettingsForm>) -> Result<Html<String>, AppError> {
    blocking(&app, move |store| {
        let config = form.into_config()?;
        store.write_config(&config)?;
        Ok(Html(views::settings(&config, true).render()?))
    })
    .await
}

/// The Discard button: retire a review ticket without landing it — done with `discarded: true`, dependents stay
/// blocked. Always a human decision (the confirm dialog says exactly what it costs); the auto-lander never does this.
pub async fn discard_ticket(State(app): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Result<Html<String>, AppError> {
    let id = TicketId(id);
    let op = Op::DiscardTicket { id: id.clone(), reason: format!("discarded from the board by {}", app.ui_owner) };
    mutate_then_detail(&app, &headers, op, id).await
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
    let op =
        Op::CreateEpic { title: form.title, color: opt(form.color), body: form.body, status: parse_status(&form.status)?, auto_merge: false };
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
    let patch = crate::ops::EpicPatch { title: Some(form.title), color: opt(form.color), body: Some(form.body), auto_merge: None };
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

/// The epic's auto-merge toggle — the same deliberate second action as the ticket's, but the grant reaches every ticket
/// filed under it at once, which is what its confirm counts out.
pub async fn epic_auto_merge(State(app): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Result<Html<String>, AppError> {
    let id = EpicId(id);
    let version = client_version(&headers)?;
    blocking(&app, move |store| {
        let on = store.read_board()?.epic(&id).ok_or_else(|| AppError::not_found(&id.to_string()))?.auto_merge;
        let patch = crate::ops::EpicPatch { auto_merge: Some(!on), ..crate::ops::EpicPatch::default() };
        ops::apply(store, Some(version), Op::UpdateEpic { id: id.clone(), patch })?;
        let tpl = views::epic_detail(&store.read_board()?, &id).ok_or_else(|| AppError::not_found(&id.to_string()))?;
        Ok(Html(tpl.render()?))
    })
    .await
}

pub async fn delete_epic(State(app): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Result<StatusCode, AppError> {
    mutate(&app, &headers, Op::DeleteEpic { id: EpicId(id) }).await
}
