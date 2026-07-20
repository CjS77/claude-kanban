//! Handler-level tests: the real router, driven with `tower::ServiceExt::oneshot` against a temp store — no sockets, no
//! browser. Covers the rendering path, the mutation funnel, the optimistic-concurrency UX, and the loopback hardening.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use claude_kanban::{
    git::git,
    ops::{self, Op},
    server::{App, router},
    store::{
        Store,
        model::{ColumnId, Effort, EpicId, External, PrRef, PrState, Status, TicketId},
    },
};
use http_body_util::BodyExt;
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:4747";

fn test_app() -> (tempfile::TempDir, Router, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join(".kanban"));
    store.init().unwrap();
    let router = router_for(&store);
    (dir, router, store)
}

fn router_for(store: &Store) -> Router {
    let (refresh, _) = tokio::sync::broadcast::channel(4);
    let app = Arc::new(App {
        store: store.clone(),
        assets_dir: None,
        allowed_hosts: vec![HOST.into()],
        allowed_origins: vec![format!("http://{HOST}")],
        title: "test".into(),
        ui_owner: "tester".into(),
        refresh,
        shutdown: tokio_util::sync::CancellationToken::new(),
    });
    router(app)
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).header(header::HOST, HOST).body(Body::empty()).unwrap()
}

fn post(path: &str, version: u64, form: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, HOST)
        .header("x-board-version", version.to_string())
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap()
}

async fn body_text(res: Response) -> String {
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

fn seed_ticket(store: &Store, title: &str) -> String {
    ops::apply(
        store,
        None,
        Op::CreateTicket { title: title.into(), body: "# Spec".into(), epic: None, labels: vec!["ui".into()], depends_on: vec![], status: Status::Ready, model: None, effort: None, auto_merge: false },
    )
    .unwrap()
    .created_ids[0]
        .clone()
}

#[tokio::test]
async fn the_board_fragment_renders_seeded_tickets_with_the_version_stamp() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "Visible on the board");
    let res = router.oneshot(get("/ui/board")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(html.contains("Visible on the board"), "{html}");
    assert!(html.contains(r#"data-version="1""#), "the fragment must carry the current version");
    assert!(html.contains(r#"data-draggable="true""#));
}

#[tokio::test]
async fn the_header_badges_the_crate_version_and_links_the_repo() {
    let (_dir, router, _store) = test_app();
    let html = body_text(router.oneshot(get("/")).await.unwrap()).await;

    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    assert!(html.contains(&format!(">{version}</span>")), "the ghost badge must show the real crate version: {html}");
    assert!(!html.contains(">claude-kanban</span>"), "the badge's old static name must be gone: {html}");
    assert!(html.contains(&format!(r#"href="{}""#, env!("CARGO_PKG_REPOSITORY"))), "the mark must link the repo: {html}");
    assert!(html.contains("<svg viewBox=\"0 0 16 16\""), "the GitHub mark is inlined, not fetched: {html}");
}

#[tokio::test]
async fn filters_hide_cards_and_disable_dragging() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "Labelled ui");
    ops::apply(
        &store,
        None,
        Op::CreateTicket { title: "Unlabelled".into(), body: String::new(), epic: None, labels: vec![], depends_on: vec![], status: Status::Draft, model: None, effort: None, auto_merge: false },
    )
    .unwrap();
    let html = body_text(router.oneshot(get("/ui/board?q=label:ui")).await.unwrap()).await;
    assert!(html.contains("Labelled ui") && !html.contains("Unlabelled"), "{html}");
    assert!(html.contains(r#"data-draggable="false""#), "a filtered board must not reorder");
}

/// Seed a ticket with a body and labels of its own — `seed_ticket` fixes both, and search is about exactly those.
fn seed_full(store: &Store, title: &str, body: &str, labels: &[&str]) -> String {
    ops::apply(
        store,
        None,
        Op::CreateTicket {
            title: title.into(),
            body: body.into(),
            epic: None,
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            depends_on: vec![],
            status: Status::Ready,
            model: None,
            effort: None,
            auto_merge: false,
        },
    )
    .unwrap()
    .created_ids[0]
        .clone()
}

#[tokio::test]
async fn the_search_bar_matches_free_text_across_every_column() {
    let (_dir, router, store) = test_app();
    for title in ["Alpha", "Beta", "Gamma", "Delta"] {
        seed_full(&store, title, "shares the phrase quantum ledger", &[]);
    }
    seed_full(&store, "Epsilon", "about something else entirely", &[]);

    // One card per column, so a bare phrase has to reach past todo.
    ops::apply(&store, None, Op::Claim { id: TicketId("K-2".into()), agent: "claude".into() }).unwrap();
    to_review_with_branch(&store, "K-3", "k-3/work");
    to_done_with_branch(&store, "K-4", "k-4/work");

    let html = body_text(router.oneshot(get("/ui/board?q=quantum+ledger")).await.unwrap()).await;
    let missing: Vec<&str> = ["Alpha", "Beta", "Gamma", "Delta"].into_iter().filter(|t| !html.contains(t)).collect();
    assert!(missing.is_empty(), "a bare phrase reaches every column, but {missing:?} did not render: {html}");
    assert!(!html.contains("Epsilon"), "a card without the phrase must go: {html}");
}

#[tokio::test]
async fn the_example_query_narrows_to_the_landed_ux_card() {
    let (_dir, router, store) = test_app();
    seed_full(&store, "Landed with the phrase", "realtime results as you type", &["UX"]);
    seed_full(&store, "Discarded with the phrase", "realtime results as you type", &["UX"]);
    seed_full(&store, "Still in review", "realtime results as you type", &["UX"]);
    seed_full(&store, "Landed without the phrase", "batch results, overnight", &["UX"]);

    to_done_with_branch(&store, "K-1", "k-1/work");
    to_review_with_branch(&store, "K-2", "k-2/work");
    ops::apply(&store, None, Op::DiscardTicket { id: TicketId("K-2".into()), reason: "superseded".into() }).unwrap();
    to_review_with_branch(&store, "K-3", "k-3/work");
    to_done_with_branch(&store, "K-4", "k-4/work");

    let html = body_text(router.oneshot(get("/ui/board?q=landed:+true,+label:+ux,+realtime+results")).await.unwrap()).await;
    assert!(html.contains("Landed with the phrase"), "the acceptance card must render: {html}");
    let leaked: Vec<&str> = ["Discarded with the phrase", "Still in review", "Landed without the phrase"]
        .into_iter()
        .filter(|t| html.contains(t))
        .collect();
    assert!(leaked.is_empty(), "only the landed ux card may survive, but {leaked:?} did too: {html}");
}

#[tokio::test]
async fn a_blank_query_keeps_the_board_draggable() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "Anything at all");

    for query in ["", "%20%20", ",,", "%20,%20", "%22%22"] {
        let html = body_text(router.clone().oneshot(get(&format!("/ui/board?q={query}"))).await.unwrap()).await;
        assert!(html.contains(r#"data-draggable="true""#), "?q={query} hides nothing, so the board must still drag: {html}");
        assert!(html.contains("Anything at all"), "?q={query} must admit every card: {html}");
    }

    let html = body_text(router.oneshot(get("/ui/board?q=label:ui")).await.unwrap()).await;
    assert!(html.contains(r#"data-draggable="false""#), "a real query must stop the drag: {html}");
}

#[tokio::test]
async fn the_filter_bar_offers_one_search_box_and_the_epic_dropdown() {
    let (_dir, router, _store) = test_app();
    let html = body_text(router.oneshot(get("/")).await.unwrap()).await;

    assert!(html.contains(r#"name="q""#), "the search box must be there: {html}");
    assert!(html.contains("search… or label:ux, status:ready, landed:true"), "with its grammar hint: {html}");
    assert!(html.contains(r#"id="filter-epic""#), "the epic dropdown survives — it is discovery, not filtering: {html}");
    // The create modals also carry a `name="status"`, so assert on the filter bar's own spelling.
    assert!(!html.contains(r#"<select name="status" class="select select-sm"#), "the status dropdown is gone: {html}");
    assert!(!html.contains(r#"name="label""#), "the label input is gone: {html}");
}

#[tokio::test]
async fn the_search_box_sits_after_the_create_buttons_inside_the_filter_bar() {
    let (_dir, router, _store) = test_app();
    let html = body_text(router.oneshot(get("/")).await.unwrap()).await;

    let filters = html.split_once(r#"id="filters""#).expect("the filter bar must carry #filters").1;
    let (bar, _) = filters.split_once("<main").expect("the filter bar closes before the board");
    let at = |needle: &str| bar.find(needle).unwrap_or_else(|| panic!("{needle} must be inside the filter bar: {bar}"));

    // hx-include="#filters" harvests input/select from #filters, so both controls have to stay within it — while the
    // create buttons, which contribute no values, sit between them.
    assert!(at(r#"id="filter-epic""#) < at("+ Epic"), "the epic dropdown leads: {bar}");
    assert!(at("+ Epic") < at(r#"name="q""#), "the search box follows the create buttons: {bar}");
    assert!(at(r#"name="q""#) < at(r#"id="search-help""#), "the help affordance trails the search box: {bar}");
}

#[tokio::test]
async fn the_search_box_wears_a_magnifier_and_a_javascript_free_help_popup() {
    let (_dir, router, _store) = test_app();
    let html = body_text(router.oneshot(get("/")).await.unwrap()).await;

    // The magnifier is a decorative inline SVG inside the label that wraps the input — no webfont, no extra asset.
    assert!(html.contains(r#"<label class="input input-sm""#), "the input is wrapped in a label: {html}");
    assert!(html.contains(r#"<circle cx="7" cy="7" r="5"/>"#), "the magnifier glass is drawn inline: {html}");

    // <details class="dropdown"> opens on click with no script — daisyUI exempts details from its hide rule.
    let help = html.split_once(r#"id="search-help""#).expect("the help popup must be there: {html}").1;
    let (popup, _) = help.split_once("</details>").expect("the help popup closes");
    assert!(popup.contains(r#"class="dropdown-content"#), "the panel is the dropdown's content: {popup}");
    assert!(!popup.contains("onclick") && !popup.contains("hx-"), "the popup must need no script to open: {popup}");

    // The keys it documents are exactly the ones search.rs answers to — `merged:` was removed in v2 and must not return.
    let keys =
        ["text:", "label:", "epic:", "id:", "note:", "status:", "col: column:", "landed:", "discarded:", "blocked:", "auto-merge:"];
    let missing: Vec<_> = keys.into_iter().filter(|key| !popup.contains(key)).collect();
    assert!(missing.is_empty(), "the popup must document every search key — missing {missing:?}: {popup}");
    assert!(!popup.contains("merged:"), "there is no merged: key: {popup}");

    // `epic:` reserves two values, and a reserved value nobody can discover may as well not exist.
    let unlisted: Vec<_> = ["none", "null"].into_iter().filter(|word| !popup.contains(word)).collect();
    assert!(unlisted.is_empty(), "the popup must spell out epic's reserved values — missing {unlisted:?}: {popup}");

    // The old title= tooltip is folded into the popup, so there is exactly one explanation of the grammar.
    assert!(!html.contains("Comma-separated. Bare text searches"), "the tooltip must not duplicate the popup: {html}");
}

#[tokio::test]
async fn a_bookmarked_label_parameter_is_inert() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "Still visible");
    let stale = body_text(router.clone().oneshot(get("/ui/board?label=ui")).await.unwrap()).await;
    let blank = body_text(router.oneshot(get("/ui/board?q=")).await.unwrap()).await;
    assert_eq!(stale, blank, "a filter that no longer exists must go inert, not error");
}

#[tokio::test]
async fn a_stale_version_conflicts_with_toast_and_corrective_refresh() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "One"); // version is now 1
    let res = router.oneshot(post("/ui/ticket", 0, "title=Stale")).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(res.headers()["hx-retarget"], "#toasts");
    assert_eq!(res.headers()["hx-reswap"], "beforeend");
    assert_eq!(res.headers()["hx-trigger"], "kanban:refresh-now");
    let html = body_text(res).await;
    assert!(html.contains("Board changed"), "{html}");
}

#[tokio::test]
async fn mutations_flow_create_move_status_delete() {
    let (_dir, router, store) = test_app();

    let res = router.clone().oneshot(post("/ui/ticket", 0, "title=New+one&status=ready&body=hello")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let res = router.clone().oneshot(post("/ui/ticket/K-1/move", 1, "to=doing&position=0")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let board = store.read_board().unwrap();
    match &board.tickets[0].column {
        claude_kanban::store::model::Column::Doing { owner, .. } => assert_eq!(owner, "tester", "UI drags stamp the human as owner"),
        other => panic!("expected doing, got {other:?}"),
    }

    // Status buttons answer with the refreshed detail pane, so the open pane never goes stale.
    let res = router.clone().oneshot(post("/ui/ticket/K-1/status", 2, "status=review")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(html.contains("review"), "{html}");

    let res = router.clone().oneshot(post("/ui/ticket/K-1/delete", 3, "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(store.read_board().unwrap().tickets.is_empty());
}

/// Seed an epic and file `titles` under it, returning the epic's id.
fn seed_epic(store: &Store, title: &str, titles: &[&str]) -> EpicId {
    let created =
        ops::apply(store, None, Op::CreateEpic { title: title.into(), color: None, body: String::new(), status: Status::Ready, auto_merge: false }).unwrap();
    let epic = EpicId(created.created_ids[0].clone());
    let file_under = |title: &&str| {
        let id = TicketId(seed_ticket(store, title));
        let patch = ops::TicketPatch { epic: Some(Some(epic.clone())), ..ops::TicketPatch::default() };
        ops::apply(store, None, Op::UpdateTicket { id, patch }).unwrap();
    };
    titles.iter().for_each(file_under);
    epic
}

#[tokio::test]
async fn deleting_an_epic_from_the_board_takes_its_tickets() {
    let (_dir, router, store) = test_app();
    seed_epic(&store, "Auth", &["Inside one", "Inside two"]);
    seed_ticket(&store, "Outside the epic");

    let version = store.read_board().unwrap().version;
    let res = router.oneshot(post("/ui/epic/EP-1/delete", version, "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let board = store.read_board().unwrap();
    let left: Vec<&str> = board.tickets.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(left, vec!["Outside the epic"], "the epic's tickets go with it, the outsider stays");
    assert!(board.epics.is_empty());
}

/// The confirm dialog is the whole safety net for an irreversible cascade, so it has to say what goes — and must no
/// longer promise the tickets survive.
#[tokio::test]
async fn the_epic_delete_confirm_says_the_tickets_go_too() {
    let (_dir, router, store) = test_app();
    seed_epic(&store, "Auth", &["Inside one", "Inside two"]);
    ops::apply(&store, None, Op::MoveTicket { id: TicketId("K-1".into()), to: ColumnId::Done, position: None, owner: None, branch: None })
        .unwrap();

    let html = body_text(router.clone().oneshot(get("/ui/epic/EP-1")).await.unwrap()).await;
    assert!(html.contains("its 2 tickets (1 already done)"), "the confirm must count the tickets and the done ones: {html}");
    assert!(html.contains("There is no undo."), "and say the cascade is final: {html}");
    assert!(!html.contains("survive, detached"), "the old promise must be gone: {html}");

    seed_epic(&store, "Empty", &[]);
    let html = body_text(router.oneshot(get("/ui/epic/EP-2")).await.unwrap()).await;
    assert!(html.contains("Delete EP-2 — Empty? It has no tickets."), "an empty epic gets the short wording: {html}");
}

/// The edit form posts every field, so a save that omits nothing must still be able to *clear* the pair — and a bad
/// effort has to be refused rather than silently dropped, which is the one way a dial like this quietly stops working.
#[tokio::test]
async fn the_edit_form_round_trips_model_and_effort_and_refuses_a_bad_level() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "hard one");
    let model_of = || store.read_board().unwrap().tickets[0].model.clone();
    let effort_of = || store.read_board().unwrap().tickets[0].effort;

    let form = "title=hard+one&body=&epic=&labels=&depends_on=&model=+claude-opus-4-8+&effort=xhigh";
    let res = router.clone().oneshot(post("/ui/ticket/K-1", 1, form)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(model_of().as_deref(), Some("claude-opus-4-8"), "free text, but trimmed");
    assert_eq!(effort_of(), Some(Effort::Xhigh));

    // An emptied model box and an "inherit" effort really do clear them.
    let form = "title=hard+one&body=&epic=&labels=&depends_on=&model=&effort=";
    let res = router.clone().oneshot(post("/ui/ticket/K-1", 2, form)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!((model_of(), effort_of()), (None, None));

    let form = "title=hard+one&body=&epic=&labels=&depends_on=&model=&effort=ludicrous";
    let res = router.clone().oneshot(post("/ui/ticket/K-1", 3, form)).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "a level that doesn't exist is a 422 toast, not a shrug");
}

#[tokio::test]
async fn the_auto_merge_toggle_flips_the_flag_and_hands_back_the_refreshed_pane() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "lands itself");
    let flagged = || store.read_board().unwrap().tickets[0].auto_merge;

    let res = router.clone().oneshot(post("/ui/ticket/K-1/auto-merge", 1, "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(flagged(), "the click writes the ticket's own flag");
    assert!(html.contains("lands itself"), "and answers with the refreshed pane, not a bare 204: {html}");
    assert!(html.contains(">auto-merge</span>"), "which now wears the warning badge: {html}");

    let version = store.read_board().unwrap().version;
    let html = body_text(router.oneshot(post("/ui/ticket/K-1/auto-merge", version, "")).await.unwrap()).await;
    assert!(!flagged(), "a second click takes it back off");
    assert!(!html.contains(">auto-merge</span>"), "and the badge goes with it: {html}");
}

/// The whole point of the dedicated route is the dialog on it, so the wording is a test, not a detail: turning it on has
/// to name the ticket and admit that main moves with nobody reviewing the merge. Turning it off carries no scare text.
#[tokio::test]
async fn the_auto_merge_confirm_names_the_ticket_and_says_main_moves_unreviewed() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "risky one");

    let html = body_text(router.clone().oneshot(get("/ui/ticket/K-1")).await.unwrap()).await;
    assert!(html.contains("Turn on auto-merge for K-1 — risky one?"), "the confirm must name the ticket: {html}");
    assert!(html.contains("fast-forward main into it with no human review of the merge"), "and what it costs: {html}");
    assert!(html.contains("There is no undo once main has moved."), "and that it is final: {html}");

    router.clone().oneshot(post("/ui/ticket/K-1/auto-merge", 1, "")).await.unwrap();
    let html = body_text(router.oneshot(get("/ui/ticket/K-1")).await.unwrap()).await;
    assert!(html.contains("Turn off auto-merge for K-1 — risky one?"), "switching it off is the safe direction: {html}");
    assert!(!html.contains("There is no undo"), "so it gets no scare text: {html}");
}

/// An epic's grant reaches its tickets without touching their stored flags, so the badge has to say where it came from —
/// and the ticket's own toggle has to admit it cannot take the epic's grant away.
#[tokio::test]
async fn a_card_credits_the_epic_when_the_grant_is_inherited() {
    let (_dir, router, store) = test_app();
    seed_epic(&store, "Auth", &["under the epic"]);
    seed_ticket(&store, "on its own");
    let patch_ticket = |id: &str, on: bool| {
        let patch = ops::TicketPatch { auto_merge: Some(on), ..ops::TicketPatch::default() };
        ops::apply(&store, None, Op::UpdateTicket { id: TicketId(id.into()), patch }).unwrap();
    };

    patch_ticket("K-2", true);
    let html = body_text(router.clone().oneshot(get("/ui/board")).await.unwrap()).await;
    assert!(html.contains(">auto-merge</span>"), "a ticket flagged in its own right wears the plain badge: {html}");
    assert!(!html.contains("auto-merge (epic)"), "nothing is inherited yet: {html}");

    let patch = ops::EpicPatch { auto_merge: Some(true), ..ops::EpicPatch::default() };
    ops::apply(&store, None, Op::UpdateEpic { id: EpicId("EP-1".into()), patch }).unwrap();
    let html = body_text(router.clone().oneshot(get("/ui/board")).await.unwrap()).await;
    assert!(html.contains(">auto-merge (epic)</span>"), "K-1 never set the flag, so the epic gets the credit: {html}");
    assert!(!store.read_board().unwrap().tickets[0].auto_merge, "and the epic's grant never wrote itself onto the ticket");

    let html = body_text(router.oneshot(get("/ui/ticket/K-1")).await.unwrap()).await;
    assert!(html.contains("Auto-merge (epic)</button>"), "the pane's button says where the grant lives: {html}");
    assert!(html.contains("still auto-merges — switch it off on the epic instead"), "and its confirm says so too: {html}");
}

/// The edit form has no auto-merge control, and `update_ticket` leaves `auto_merge: None` in its patch precisely so that
/// an unrelated save cannot quietly clear a dangerous flag. That `None` is the guarantee this test defends.
#[tokio::test]
async fn an_ordinary_edit_form_save_leaves_auto_merge_alone() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "flagged");
    router.clone().oneshot(post("/ui/ticket/K-1/auto-merge", 1, "")).await.unwrap();
    assert!(store.read_board().unwrap().tickets[0].auto_merge, "precondition: the flag is on");

    let version = store.read_board().unwrap().version;
    let form = "title=flagged+and+renamed&body=&epic=&labels=&depends_on=&model=&effort=";
    assert_eq!(router.oneshot(post("/ui/ticket/K-1", version, form)).await.unwrap().status(), StatusCode::OK);

    let board = store.read_board().unwrap();
    assert_eq!(board.tickets[0].title, "flagged and renamed", "the save landed");
    assert!(board.tickets[0].auto_merge, "and it left the one field it carries no control for untouched");
}

#[tokio::test]
async fn the_epic_auto_merge_confirm_counts_the_tickets_the_grant_reaches() {
    let (_dir, router, store) = test_app();
    seed_epic(&store, "Auth", &["one", "two"]);

    let html = body_text(router.clone().oneshot(get("/ui/epic/EP-1")).await.unwrap()).await;
    assert!(html.contains("Turn on auto-merge for EP-1 — Auth and its 2 tickets?"), "one click covers the list: {html}");

    let version = store.read_board().unwrap().version;
    let res = router.oneshot(post("/ui/epic/EP-1/auto-merge", version, "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(store.read_board().unwrap().epics[0].auto_merge, "the toggle flips the epic's own flag");
    let html = body_text(res).await;
    assert!(html.contains(">auto-merge</span>"), "the refreshed epic pane wears the badge: {html}");
    assert!(html.contains("Turn off auto-merge for EP-1 — Auth?"), "and the confirm has flipped with it: {html}");
}

#[tokio::test]
async fn loopback_hardening_rejects_what_it_should() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "target");

    // Wrong Host (DNS rebinding shape) — refused even for reads.
    let req = Request::builder().uri("/ui/board").header(header::HOST, "evil.example:4747").body(Body::empty()).unwrap();
    assert_eq!(router.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);

    // Mutation without the custom header (cross-site form shape) — refused.
    let req = Request::builder()
        .method("POST")
        .uri("/ui/ticket/K-1/delete")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    assert_eq!(router.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);

    // Mutation with a foreign Origin — refused even with the header present.
    let req = Request::builder()
        .method("POST")
        .uri("/ui/ticket/K-1/delete")
        .header(header::HOST, HOST)
        .header("x-board-version", "1")
        .header(header::ORIGIN, "https://evil.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(router.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);

    assert_eq!(store.read_board().unwrap().tickets.len(), 1, "nothing got through");
}

#[tokio::test]
async fn raw_markdown_is_plain_text_and_assets_are_embedded() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "with body");

    let res = router.clone().oneshot(get("/raw/ticket/K-1")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert_eq!(body_text(res).await, "# Spec");

    let res = router.clone().oneshot(get("/assets/glue.js")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/javascript; charset=utf-8");
    assert!(body_text(res).await.contains("X-Board-Version"), "embedded glue.js must be the real one");

    let res = router.oneshot(get("/assets/../Cargo.toml")).await.unwrap();
    assert_ne!(res.status(), StatusCode::OK, "no traversal");
}

#[tokio::test]
async fn detail_and_edit_panes_render_and_404_cleanly() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "Detailed");

    let html = body_text(router.clone().oneshot(get("/ui/ticket/K-1")).await.unwrap()).await;
    assert!(html.contains("Detailed") && html.contains("/raw/ticket/K-1"), "{html}");

    let html = body_text(router.clone().oneshot(get("/ui/ticket/K-1/edit")).await.unwrap()).await;
    assert!(html.contains("# Spec"), "edit form shows the raw markdown: {html}");

    let res = router.oneshot(get("/ui/ticket/K-99")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(res.headers()["hx-retarget"], "#toasts", "a missing ticket toasts instead of breaking the pane");
}

/// Walk a seeded ticket to `review` carrying `branch`, the shape `pr::eligible` inspects.
fn to_review_with_branch(store: &Store, id: &str, branch: &str) {
    let id = TicketId(id.into());
    ops::apply(store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
    ops::apply(store, None, Op::StampWorktree { id: id.clone(), branch: branch.into(), path: "/tmp/unused".into() }).unwrap();
    ops::apply(store, None, Op::MoveTicket { id, to: ColumnId::Review, position: None, owner: None, branch: None }).unwrap();
}

/// Walk a seeded ticket all the way to `done` carrying `branch` — the merged-badge shape.
fn to_done_with_branch(store: &Store, id: &str, branch: &str) {
    to_review_with_branch(store, id, branch);
    ops::apply(store, None, Op::MoveTicket { id: TicketId(id.into()), to: ColumnId::Done, position: None, owner: None, branch: None })
        .unwrap();
}

#[tokio::test]
async fn the_create_pr_button_tracks_eligibility_live() {
    // The store's parent is a real repository with the ticket's branch; the remote arrives mid-test.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]).unwrap();
    git(repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "seed"]).unwrap();
    git(repo, &["branch", "k-1/work"]).unwrap();
    let store = Store::at(repo.join(".kanban"));
    store.init().unwrap();
    let router = router_for(&store);

    seed_ticket(&store, "In review with branch");
    to_review_with_branch(&store, "K-1", "k-1/work");

    // Review + branch, but no remote: no button.
    let html = body_text(router.clone().oneshot(get("/ui/ticket/K-1")).await.unwrap()).await;
    assert!(!html.contains("Create PR"), "{html}");

    // A remote added mid-session shows the button without a server restart.
    git(repo, &["remote", "add", "origin", "https://example.invalid/repo.git"]).unwrap();
    let html = body_text(router.clone().oneshot(get("/ui/ticket/K-1")).await.unwrap()).await;
    assert!(html.contains("Create PR") && html.contains("/ui/ticket/K-1/create-pr"), "{html}");

    // A todo ticket never shows it, remote or not.
    seed_ticket(&store, "Still todo");
    let html = body_text(router.clone().oneshot(get("/ui/ticket/K-2")).await.unwrap()).await;
    assert!(!html.contains("Create PR"), "{html}");

    // Clicking it on the branchless todo ticket refuses with a toast, not a push.
    let res = router.clone().oneshot(post("/ui/ticket/K-2/create-pr", 5, "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(res.headers()["hx-retarget"], "#toasts");
    let toast = body_text(res).await;
    assert!(toast.contains("not a review ticket with a branch"), "{toast}");

    // A done ticket has landed — the PR moment is over, no button however complete its data.
    git(repo, &["branch", "k-3/landed"]).unwrap();
    seed_ticket(&store, "Already landed");
    to_done_with_branch(&store, "K-3", "k-3/landed");
    let html = body_text(router.clone().oneshot(get("/ui/ticket/K-3")).await.unwrap()).await;
    assert!(!html.contains("Create PR"), "{html}");

    // Deleting the local branch (merged and cleaned up) hides the button again.
    git(repo, &["branch", "-D", "k-1/work"]).unwrap();
    let html = body_text(router.oneshot(get("/ui/ticket/K-1")).await.unwrap()).await;
    assert!(!html.contains("Create PR"), "{html}");
}

#[tokio::test]
async fn done_tickets_all_show_whatever_their_branch_did() {
    // The store's parent is a real repository, so the git state the withdrawn merged surface used to read is really
    // there — and must now change nothing about the render.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let commit = |msg: &str| {
        let sign = ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"];
        let args: Vec<&str> = sign.iter().chain(&["commit", "--allow-empty", "-q", "-m", msg]).copied().collect();
        git(repo, &args).unwrap();
    };
    git(repo, &["init", "-q", "-b", "main"]).unwrap();
    commit("seed");
    let store = Store::at(repo.join(".kanban"));
    store.init().unwrap();
    let router = router_for(&store);

    // K-1's branch does not exist locally — the merged-and-deleted arm. K-2's branch carries an unmerged commit.
    seed_ticket(&store, "Merged and deleted");
    to_done_with_branch(&store, "K-1", "k-1/gone");
    git(repo, &["checkout", "-q", "-b", "k-2/alive"]).unwrap();
    commit("work");
    git(repo, &["checkout", "-q", "main"]).unwrap();
    seed_ticket(&store, "Branch still alive");
    to_done_with_branch(&store, "K-2", "k-2/alive");

    // Both are done, so both show: done is the record of what shipped, with no badge and no hidden-card hint.
    let html = body_text(router.clone().oneshot(get("/ui/board")).await.unwrap()).await;
    assert!(html.contains("Merged and deleted") && html.contains("Branch still alive"), "{html}");
    assert!(!html.contains("merged</span>") && !html.contains("#a855f7"), "no merged badge survives: {html}");
    assert!(!html.contains("+1 merged"), "no hidden-card hint: {html}");
    assert!(html.contains(r#"data-draggable="true""#), "an unfiltered board still drags: {html}");

    // A bookmarked ?merged=1 is inert, not an error: serde ignores the unknown field.
    let stale = body_text(router.oneshot(get("/ui/board?merged=1")).await.unwrap()).await;
    assert_eq!(stale, html, "the stale parameter changes nothing");
}

#[tokio::test]
async fn the_review_column_renders_with_pr_and_branch_state_badges() {
    // A real repo so branch-existence answers; the review column sits between Doing and Done and its cards carry the
    // PR lifecycle badges plus the branch-gone flag.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]).unwrap();
    git(repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "seed"]).unwrap();
    git(repo, &["checkout", "-q", "-b", "k-1/alive"]).unwrap();
    git(repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "work"]).unwrap();
    git(repo, &["checkout", "-q", "main"]).unwrap();
    let store = Store::at(repo.join(".kanban"));
    store.init().unwrap();
    let router = router_for(&store);

    seed_ticket(&store, "Open PR here");
    to_review_with_branch(&store, "K-1", "k-1/alive");
    ops::apply(
        &store,
        None,
        Op::SetPr {
            id: TicketId("K-1".into()),
            pr: Some(PrRef { number: 7, url: "https://github.com/x/y/pull/7".into(), state: PrState::Open, merged_commit: None }),
        },
    )
    .unwrap();
    seed_ticket(&store, "Merged awaiting pull");
    to_review_with_branch(&store, "K-2", "k-2/pushed"); // never a local ref → also wears "branch gone"
    ops::apply(
        &store,
        None,
        Op::SetPr {
            id: TicketId("K-2".into()),
            pr: Some(PrRef { number: 8, url: "https://github.com/x/y/pull/8".into(), state: PrState::Merged, merged_commit: Some("0".repeat(40)) }),
        },
    )
    .unwrap();

    let html = body_text(router.clone().oneshot(get("/ui/board")).await.unwrap()).await;
    let (doing_at, review_at, done_at) =
        (html.find(">Doing<").unwrap(), html.find(">Review<").unwrap(), html.find(">Done<").unwrap());
    assert!(doing_at < review_at && review_at < done_at, "review sits between doing and done");
    assert!(html.contains(">PR #7</a>"), "{html}");
    assert!(html.contains("PR #8 merged — pull main"), "{html}");
    assert!(html.contains(">branch gone</span>"), "{html}");
    assert!(html.contains("k-1/alive"), "review cards show their branch: {html}");

    // The detail pane links the PR and offers Discard on review tickets.
    let html = body_text(router.clone().oneshot(get("/ui/ticket/K-1")).await.unwrap()).await;
    assert!(html.contains(r#"href="https://github.com/x/y/pull/7""#) && html.contains("/ui/ticket/K-1/discard"), "{html}");
}

#[tokio::test]
async fn discard_closes_the_ticket_and_keeps_dependents_blocked_on_the_board() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "Doomed work");
    ops::apply(
        &store,
        None,
        Op::CreateTicket { title: "Blocked follow-up".into(), body: String::new(), epic: None, labels: vec![], depends_on: vec![TicketId("K-1".into())], status: Status::Ready, model: None, effort: None, auto_merge: false },
    )
    .unwrap();
    to_review_with_branch(&store, "K-1", "k-1/doomed");

    let res = router.clone().oneshot(post("/ui/ticket/K-1/discard", 5, "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let pane = body_text(res).await;
    assert!(pane.contains(">discarded</span>"), "{pane}");

    let html = body_text(router.clone().oneshot(get("/ui/board")).await.unwrap()).await;
    assert!(html.contains(">discarded</span>") && html.contains(">blocked</span>"), "{html}");
    assert!(!html.contains(">merged</span>"), "no card wears a merged badge — the surface is gone: {html}");

    // Discarding anything not in review refuses with a toast.
    let res = router.oneshot(post("/ui/ticket/K-2/discard", 6, "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn dragging_a_card_to_review_closes_it_out() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "Dragged along");
    ops::apply(&store, None, Op::Claim { id: TicketId("K-1".into()), agent: "claude".into() }).unwrap();

    let res = router.oneshot(post("/ui/ticket/K-1/move", 2, "to=review&position=0")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let board = store.read_board().unwrap();
    assert!(matches!(board.tickets[0].column, claude_kanban::store::model::Column::Review { .. }));
    assert!(store.read_claims().unwrap().is_empty(), "entering review drops the claim");
}

#[tokio::test]
async fn done_cards_carry_no_merged_badge() {
    // An external ticket's branch is whatever the delegate created on the far side and never existed locally — the
    // shape that most tempted the withdrawn badge into guessing. Done cards now say nothing about merge state at all.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]).unwrap();
    git(repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "seed"]).unwrap();
    let store = Store::at(repo.join(".kanban"));
    store.init().unwrap();
    let router = router_for(&store);

    seed_ticket(&store, "Delegated elsewhere");
    ops::apply(
        &store,
        None,
        Op::BindExternal {
            id: TicketId("K-1".into()),
            external: Some(External { provider: "github".into(), kind: "issue".into(), number: 42 }),
        },
    )
    .unwrap();
    // The daemon's branch name lands on the column by hand-edit shape: no local branch of that name exists.
    store
        .mutate(None, |board, _| {
            board.tickets[0].column =
                claude_kanban::store::model::Column::Done { branch: Some("myrepo-issue0042".into()), completed_at: chrono::Utc::now(), discarded: false };
            Ok::<_, claude_kanban::store::StoreError>(())
        })
        .unwrap();

    let html = body_text(router.oneshot(get("/ui/board")).await.unwrap()).await;
    assert!(html.contains("Delegated elsewhere"), "{html}");
    assert!(!html.contains(">merged</span>") && !html.contains("+1 merged"), "{html}");
}

#[tokio::test]
async fn a_done_ticket_outside_a_git_repo_renders_with_no_pr_button() {
    // The plain temp store is not a git repository — eligibility must answer false, never error the pane.
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "Done, repo-less");
    to_done_with_branch(&store, "K-1", "k-1/work");

    let res = router.clone().oneshot(get("/ui/ticket/K-1")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(!html.contains("Create PR"), "{html}");

    // And the POST refuses with a 422 toast rather than pushing from nowhere.
    let res = router.oneshot(post("/ui/ticket/K-1/create-pr", 3, "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(res.headers()["hx-retarget"], "#toasts");
}

#[tokio::test]
async fn the_default_port_hunts_but_an_explicit_port_fails_loudly() {
    use claude_kanban::server::{Bound, bind_listener};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join(".kanban"));
    store.init().unwrap();

    // Another project holds the would-be default port.
    let stranger = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = stranger.local_addr().unwrap().port();

    // No explicit choice and no serve.pid → hunt to a free port…
    let Bound::Listener(listener) = bind_listener(&store, None, taken).await.unwrap() else {
        panic!("without a live serve.pid the default must hunt, not report a running serve");
    };
    let port = listener.local_addr().unwrap().port();
    assert_ne!(port, taken);

    // …and the hunted port genuinely serves: a real socket, a real request.
    let (refresh, _) = tokio::sync::broadcast::channel(4);
    let app = Arc::new(App {
        store: store.clone(),
        assets_dir: None,
        allowed_hosts: vec![format!("127.0.0.1:{port}")],
        allowed_origins: vec![format!("http://127.0.0.1:{port}")],
        title: "test".into(),
        ui_owner: "tester".into(),
        refresh,
        shutdown: tokio_util::sync::CancellationToken::new(),
    });
    tokio::spawn(async move { axum::serve(listener, router(app)).await });
    let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    conn.write_all(format!("GET /ui/board HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut response = String::new();
    conn.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "the hunted port must answer /ui/board: {response}");

    // An explicit port that is taken stays a loud failure, hinting at the live serve the pid file names.
    std::fs::write(store.dir().join("serve.pid"), format!(r#"{{"pid": {}, "port": {port}}}"#, std::process::id())).unwrap();
    let err = bind_listener(&store, Some(taken), taken).await.unwrap_err();
    assert!(format!("{err:#}").contains("already seems to be running"), "{err:#}");

    // The default port taken while *this* store's serve.pid names a live process → report it, don't duplicate.
    let Bound::AlreadyServed { pid, port: reported } = bind_listener(&store, None, taken).await.unwrap() else {
        panic!("a live serve.pid must be reported, not duplicated");
    };
    assert_eq!((pid, reported), (std::process::id(), port));

    // A dead pid is a stale file: hunt again instead of refusing to serve.
    std::fs::write(store.dir().join("serve.pid"), r#"{"pid": 4009999999, "port": 4747}"#).unwrap();
    assert!(matches!(bind_listener(&store, None, taken).await.unwrap(), Bound::Listener(_)));
}

#[tokio::test]
async fn the_settings_pane_round_trips_the_config_and_refuses_garbage() {
    let (_dir, router, store) = test_app();

    // GET shows what init seeded — main_branch pinned, poll_interval 60.
    let html = body_text(router.clone().oneshot(get("/ui/settings")).await.unwrap()).await;
    assert!(html.contains(r#"name="main_branch" value="main""#), "{html}");
    assert!(html.contains(r#"name="poll_interval" value="60""#), "{html}");
    assert!(html.contains("/ui/settings"), "{html}");

    // POST writes the whole file; the re-rendered pane confirms and carries the new values.
    let form = "main_branch=trunk&poll_interval=0&max_workers=3&idle_time=&worktree_root=%2Fdata%2Fwt&copy_to_worktrees=.env%0Acerts%2Flocal.pem&port=";
    let res = router.clone().oneshot(post("/ui/settings", 1, form)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(html.contains("Saved") && html.contains(r#"value="trunk""#), "{html}");

    let config = claude_kanban::config::Config::load(store.dir()).unwrap();
    assert_eq!(config.main_branch.as_deref(), Some("trunk"));
    assert_eq!(config.poll_interval, Some(0), "0 is stored verbatim — it is the off switch");
    assert_eq!(config.max_workers(), 3);
    assert_eq!(config.idle_time(), 300, "cleared field falls back to the default");
    assert_eq!(config.copy_to_worktrees, vec![".env", "certs/local.pem"]);
    assert!(config.port.is_none(), "empty port stays 'nobody chose'");

    // A non-numeric number is a 422 toast and the file stays as-saved.
    let res = router.clone().oneshot(post("/ui/settings", 2, "max_workers=lots")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(res.headers()["hx-retarget"], "#toasts");
    assert_eq!(claude_kanban::config::Config::load(store.dir()).unwrap().max_workers(), 3, "bad input must not clobber the file");

    // The guard covers settings like every mutation: no version header, no write.
    let req = Request::builder().method("POST").uri("/ui/settings").header(header::HOST, HOST).body(Body::empty()).unwrap();
    assert_eq!(router.oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_header_carries_the_settings_gear() {
    let (_dir, router, _store) = test_app();
    let html = body_text(router.oneshot(get("/")).await.unwrap()).await;
    assert!(html.contains(r#"hx-get="/ui/settings""#), "{html}");
}

#[tokio::test]
async fn the_poller_lands_merged_review_tickets_and_stops_on_shutdown() {
    // A real repo whose review ticket's branch has already merged into main: the poller's startup sweep lands it, the
    // file watcher broadcasts the write (serve never signals its own writes in-process), and cancellation ends the task.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]).unwrap();
    git(repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "seed"]).unwrap();
    git(repo, &["branch", "k-1/work"]).unwrap(); // at main's tip: already an ancestor, i.e. merged
    let store = Store::at(repo.join(".kanban"));
    store.init().unwrap();
    seed_ticket(&store, "waiting to land");
    to_review_with_branch(&store, "K-1", "k-1/work");

    let shutdown = tokio_util::sync::CancellationToken::new();
    let refresh = claude_kanban::server::sse::spawn_watcher(store.clone(), shutdown.clone()).unwrap();
    let mut rx = refresh.subscribe();
    let poller = claude_kanban::server::spawn_poller(store.clone(), shutdown.clone(), Some(std::time::Duration::from_millis(10)));

    let version = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("the landing must reach the SSE channel")
        .unwrap();
    assert!(version > 0);
    let board = store.read_board().unwrap();
    assert!(
        matches!(board.tickets[0].column, claude_kanban::store::model::Column::Done { discarded: false, .. }),
        "the poller's sweep lands the merged ticket"
    );

    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), poller).await.expect("cancellation must end the poller").unwrap();
}

#[tokio::test]
async fn the_file_watcher_broadcasts_on_store_writes() {
    let (_dir, _router, store) = test_app();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let refresh = claude_kanban::server::sse::spawn_watcher(store.clone(), shutdown.clone()).unwrap();
    let mut rx = refresh.subscribe();

    seed_ticket(&store, "triggers an event");
    let version = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("watcher must notice the write")
        .unwrap();
    assert_eq!(version, 1);
    shutdown.cancel();
}

/// Seed a ticket carrying its own `auto_merge` answer.
fn seed_auto_merging(store: &Store, title: &str, auto_merge: bool) {
    ops::apply(
        store,
        None,
        Op::CreateTicket {
            title: title.into(),
            body: String::new(),
            epic: None,
            labels: vec![],
            depends_on: vec![],
            status: Status::Ready,
            model: None,
            effort: None,
            auto_merge,
        },
    )
    .unwrap();
}

#[tokio::test]
async fn the_auto_merge_filter_splits_the_board_both_ways() {
    let (_dir, router, store) = test_app();
    seed_auto_merging(&store, "Merges itself", true);
    seed_auto_merging(&store, "Waits for a human", false);

    let html = body_text(router.clone().oneshot(get("/ui/board?q=auto-merge:true")).await.unwrap()).await;
    assert!(html.contains("Merges itself"), "the flagged card must survive: {html}");
    assert!(!html.contains("Waits for a human"), "an unflagged card must go: {html}");

    let html = body_text(router.oneshot(get("/ui/board?q=auto-merge:false")).await.unwrap()).await;
    assert!(html.contains("Waits for a human"), "false selects the rest, rather than nothing: {html}");
    assert!(!html.contains("Merges itself"), "and only the rest: {html}");
}

/// The filter reads the derived flag, so an epic's permission reaches the tickets filed under it.
#[tokio::test]
async fn auto_merge_true_finds_a_ticket_that_inherits_the_flag_from_its_epic() {
    let (_dir, router, store) = test_app();
    let created =
        ops::apply(&store, None, Op::CreateEpic { title: "Chores".into(), color: None, body: String::new(), status: Status::Ready, auto_merge: true })
            .unwrap();
    let epic = EpicId(created.created_ids[0].clone());
    let id = TicketId(seed_ticket(&store, "Inherits from its epic"));
    let patch = ops::TicketPatch { epic: Some(Some(epic)), ..ops::TicketPatch::default() };
    ops::apply(&store, None, Op::UpdateTicket { id: id.clone(), patch }).unwrap();
    seed_auto_merging(&store, "Filed under nothing", false);

    let html = body_text(router.oneshot(get("/ui/board?q=auto-merge:true")).await.unwrap()).await;
    assert!(html.contains("Inherits from its epic"), "the epic's flag must reach its ticket: {html}");
    assert!(!store.read_board().unwrap().ticket(&id).unwrap().auto_merge, "though the ticket's own flag stays false");
    assert!(!html.contains("Filed under nothing"), "a ticket under no epic keeps its own answer: {html}");
}
