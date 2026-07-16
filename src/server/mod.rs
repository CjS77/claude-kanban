//! `claude-kanban serve` — the human's face of the board: server-rendered HTML (Askama), swapped by htmx, styled by
//! Tailwind + daisyUI, live-updated over SSE. Bound to loopback, hardened in [`security`], and *thin*: reads render the
//! read model, writes funnel through [`crate::ops`] exactly like the MCP server's do.

pub mod assets;
pub mod routes;
pub mod security;
pub mod sse;
pub mod views;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{Router, middleware, routing::get, routing::post};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

use crate::store::Store;

/// Shared state for every handler.
#[derive(Debug)]
pub struct App {
    pub store: Store,
    /// Dev override: serve web assets from this directory instead of the embedded copies.
    pub assets_dir: Option<PathBuf>,
    /// Exact `Host` header values this server answers to (DNS-rebinding defence — see [`security`]).
    pub allowed_hosts: Vec<String>,
    /// Exact `Origin` values allowed on mutations.
    pub allowed_origins: Vec<String>,
    /// Shown in the header: the repo directory's name.
    pub title: String,
    /// Who a browser drag into `doing` records as the owner: `git config user.name`, else `$USER`, else `"user"`.
    pub ui_owner: String,
    /// The live-update channel; SSE handlers subscribe, the file watcher publishes.
    pub refresh: broadcast::Sender<u64>,
    /// Trips on ctrl-c so open SSE streams end instead of wedging graceful shutdown.
    pub shutdown: CancellationToken,
}

pub type AppState = Arc<App>;

/// Run the server until ctrl-c. Fails fast — before binding — if the store can't produce a valid board.
pub fn serve(store: Store, port: u16, no_open: bool, assets_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(run(store, port, no_open, assets_dir))
}

async fn run(store: Store, port: u16, no_open: bool, assets_dir: Option<PathBuf>) -> anyhow::Result<()> {
    store.read_board().context("cannot serve this store")?;

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .with_context(|| bind_failure_hint(&store, port))?;
    let port = listener.local_addr()?.port();

    let shutdown = CancellationToken::new();
    let refresh = sse::spawn_watcher(store.clone(), shutdown.clone()).context("could not watch the store for changes")?;
    let app: AppState = Arc::new(App {
        title: project_title(&store),
        ui_owner: ui_owner(&store),
        allowed_hosts: vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        allowed_origins: vec![format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")],
        store: store.clone(),
        assets_dir,
        refresh,
        shutdown: shutdown.clone(),
    });

    let pid_file = store.dir().join("serve.pid");
    let _ = std::fs::write(&pid_file, serde_json::json!({ "pid": std::process::id(), "port": port }).to_string());

    let url = format!("http://127.0.0.1:{port}/");
    println!("Serving the board on {url}  (ctrl-c to stop)");
    tracing::info!(%url, store = %store.dir().display(), "board UI listening");
    if !no_open {
        let _ = open::that_detached(&url);
    }

    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("ctrl-c — shutting down");
            shutdown.cancel();
        }
    });

    let result = axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await;
    let _ = std::fs::remove_file(&pid_file);
    result.map_err(Into::into)
}

/// The full route table. Public so handler tests can drive it with `tower::ServiceExt::oneshot`.
pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/", get(routes::page))
        .route("/ui/board", get(routes::board))
        .route("/ui/ticket", post(routes::create_ticket))
        .route("/ui/ticket/{id}", get(routes::ticket_detail).post(routes::update_ticket))
        .route("/ui/ticket/{id}/edit", get(routes::ticket_edit))
        .route("/ui/ticket/{id}/move", post(routes::move_ticket))
        .route("/ui/ticket/{id}/status", post(routes::ticket_status))
        .route("/ui/ticket/{id}/note", post(routes::add_note))
        .route("/ui/ticket/{id}/delete", post(routes::delete_ticket))
        .route("/ui/epic", post(routes::create_epic))
        .route("/ui/epic/{id}", get(routes::epic_detail).post(routes::update_epic))
        .route("/ui/epic/{id}/edit", get(routes::epic_edit))
        .route("/ui/epic/{id}/status", post(routes::epic_status))
        .route("/ui/epic/{id}/delete", post(routes::delete_epic))
        .route("/raw/ticket/{id}", get(routes::raw_ticket))
        .route("/raw/epic/{id}", get(routes::raw_epic))
        .route("/events", get(sse::events))
        .route("/assets/{*path}", get(assets::asset))
        .layer(middleware::from_fn_with_state(app.clone(), security::guard))
        .layer(TraceLayer::new_for_http())
        .with_state(app)
}

/// A friendlier bind error: if a pid file exists, another `serve` is probably already running.
fn bind_failure_hint(store: &Store, port: u16) -> String {
    let pid_file = store.dir().join("serve.pid");
    match std::fs::read_to_string(&pid_file).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
        Some(info) => format!(
            "could not bind port {port} — a serve already seems to be running (pid {}, port {}; {} if it isn't)",
            info["pid"],
            info["port"],
            pid_file.display()
        ),
        None => format!("could not bind 127.0.0.1:{port}"),
    }
}

/// The board's title: the repo directory's name (the store lives at `<repo>/.kanban`).
fn project_title(store: &Store) -> String {
    store
        .dir()
        .parent()
        .and_then(|repo| repo.file_name())
        .map_or_else(|| "kanban".to_owned(), |name| name.to_string_lossy().into_owned())
}

/// Who a UI drag into `doing` names as owner.
fn ui_owner(store: &Store) -> String {
    let repo = store.dir().parent().map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
    crate::git::git(&repo, &["config", "user.name"])
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| "user".to_owned())
}
