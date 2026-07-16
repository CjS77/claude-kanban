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
    ops::{self, Op},
    server::{App, router},
    store::{Store, model::Status},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:4747";

fn test_app() -> (tempfile::TempDir, Router, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join(".kanban"));
    store.init().unwrap();
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
    (dir, router(app), store)
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
        Op::CreateTicket { title: title.into(), body: "# Spec".into(), epic: None, labels: vec!["ui".into()], depends_on: vec![], status: Status::Ready },
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
async fn filters_hide_cards_and_disable_dragging() {
    let (_dir, router, store) = test_app();
    seed_ticket(&store, "Labelled ui");
    ops::apply(
        &store,
        None,
        Op::CreateTicket { title: "Unlabelled".into(), body: String::new(), epic: None, labels: vec![], depends_on: vec![], status: Status::Draft },
    )
    .unwrap();
    let html = body_text(router.oneshot(get("/ui/board?label=ui")).await.unwrap()).await;
    assert!(html.contains("Labelled ui") && !html.contains("Unlabelled"), "{html}");
    assert!(html.contains(r#"data-draggable="false""#), "a filtered board must not reorder");
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
