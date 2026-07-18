//! `claude-kanban serve` — the human's face of the board: server-rendered HTML (Askama), swapped by htmx, styled by
//! Tailwind + daisyUI, live-updated over SSE. Bound to loopback, hardened in [`security`], and *thin*: reads render the
//! read model, writes funnel through [`crate::ops`] exactly like the MCP server's do.

pub mod assets;
pub mod routes;
pub mod search;
pub mod security;
pub mod sse;
pub mod views;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{Router, middleware, routing::get, routing::post};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

use crate::{config::Config, store::Store};

/// The UI port when nothing chooses one explicitly. Not a hard address: taken by another project, `serve` hunts.
pub const DEFAULT_PORT: u16 = 4747;

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
/// `port: None` means nobody chose one (flag or env): try [`DEFAULT_PORT`], hunting for a free port when it's taken.
pub fn serve(store: Store, port: Option<u16>, no_open: bool, assets_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(run(store, port, no_open, assets_dir))
}

async fn run(store: Store, port: Option<u16>, no_open: bool, assets_dir: Option<PathBuf>) -> anyhow::Result<()> {
    store.read_board().context("cannot serve this store")?;

    let explicit = Config::load(store.dir())?.port(port);
    let listener = match bind_listener(&store, explicit, DEFAULT_PORT).await? {
        Bound::Listener(listener) => listener,
        Bound::AlreadyServed { pid, port } => {
            let url = format!("http://127.0.0.1:{port}/");
            println!("This board is already being served on {url} (pid {pid}) — not starting a duplicate.");
            open_browser(&url, no_open);
            return Ok(());
        }
    };
    let port = listener.local_addr()?.port();

    let shutdown = CancellationToken::new();
    let refresh = sse::spawn_watcher(store.clone(), shutdown.clone()).context("could not watch the store for changes")?;
    let _poller = spawn_poller(store.clone(), shutdown.clone(), None);
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
    open_browser(&url, no_open);

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

/// The landing loop: one immediate offline sweep at startup (a board served after an offline `git pull` corrects
/// itself before anyone looks), then per tick a sweep and — the serve face's second sanctioned network egress,
/// config-gated — the gh PR poll. The config re-reads every tick, so the settings pane's changes to `poll_interval`
/// (including turning polling off, or back on) apply without a restart; while disabled the loop re-checks each minute.
///
/// Every write the passes make goes through `ops::apply`, so the file watcher broadcasts them like any other mutation —
/// serve still never signals its own writes in-process. Errors are logged and never fatal: the next tick starts fresh.
/// `interval_override` pins the cadence for tests; production passes `None` and follows the config.
#[must_use]
pub fn spawn_poller(store: Store, shutdown: CancellationToken, interval_override: Option<std::time::Duration>) -> tokio::task::JoinHandle<()> {
    const DISABLED_RECHECK: std::time::Duration = std::time::Duration::from_secs(60);
    tokio::spawn(async move {
        run_pass(&store, false).await;
        loop {
            let configured = Config::load(store.dir()).ok().and_then(|c| c.poll_interval());
            let (delay, enabled) = match interval_override.or(configured) {
                Some(interval) => (interval, true),
                None => (DISABLED_RECHECK, false),
            };
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(delay) => {}
            }
            if enabled {
                run_pass(&store, true).await;
            }
        }
        tracing::debug!("landing poller stopped");
    })
}

/// One poller tick: the offline sweep, then (when asked) the gh poll — both blocking, both off the async threads.
async fn run_pass(store: &Store, with_gh: bool) {
    let store = store.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let swept = crate::land::sweep(&store)?;
        let polled = if with_gh { crate::land::poll(&store)? } else { 0 };
        anyhow::Ok((swept, polled))
    })
    .await;
    match outcome {
        Ok(Ok((swept, polled))) if swept > 0 || polled > 0 => tracing::info!(swept, polled, "landing pass"),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "landing pass failed — retrying next tick"),
        Err(e) => tracing::warn!(error = %e, "landing pass panicked"),
    }
}

/// Show the board to the user, unless `no_open` says they didn't ask for it. Returns whether the open was attempted —
/// both the fresh-bind and the already-served paths call this, so `serve` ends with a browser on the board either way.
fn open_browser(url: &str, no_open: bool) -> bool {
    if no_open {
        tracing::debug!(%url, "not opening a browser — --no-open");
        return false;
    }
    let _ = open::that_detached(url);
    true
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
        .route("/ui/ticket/{id}/create-pr", post(routes::create_pr))
        .route("/ui/ticket/{id}/discard", post(routes::discard_ticket))
        .route("/ui/ticket/{id}/delete", post(routes::delete_ticket))
        .route("/ui/epic", post(routes::create_epic))
        .route("/ui/epic/{id}", get(routes::epic_detail).post(routes::update_epic))
        .route("/ui/epic/{id}/edit", get(routes::epic_edit))
        .route("/ui/epic/{id}/status", post(routes::epic_status))
        .route("/ui/epic/{id}/delete", post(routes::delete_epic))
        .route("/ui/settings", get(routes::settings).post(routes::save_settings))
        .route("/raw/ticket/{id}", get(routes::raw_ticket))
        .route("/raw/epic/{id}", get(routes::raw_epic))
        .route("/events", get(sse::events))
        .route("/assets/{*path}", get(assets::asset))
        .layer(middleware::from_fn_with_state(app.clone(), security::guard))
        .layer(TraceLayer::new_for_http())
        .with_state(app)
}

/// Where the serve socket comes from: a freshly bound listener, or the discovery that this store is already served.
#[derive(Debug)]
pub enum Bound {
    /// Bound and ready to serve.
    Listener(tokio::net::TcpListener),
    /// This store's `serve.pid` names a live process — starting a duplicate would be noise, not service.
    AlreadyServed { pid: u32, port: u16 },
}

/// Choose and bind the serve socket. An explicit port (flag / env / config) is honoured or fails loudly with the hint —
/// an explicit choice is never silently overridden. No choice tries `default_port`; when that's taken, either this store
/// is already being served (report it, don't duplicate) or another project owns the port (let the OS pick a free one).
pub async fn bind_listener(store: &Store, explicit: Option<u16>, default_port: u16) -> anyhow::Result<Bound> {
    if let Some(port) = explicit {
        let listener = bind(port).await.with_context(|| bind_failure_hint(store, port))?;
        return Ok(Bound::Listener(listener));
    }
    match bind(default_port).await {
        Ok(listener) => Ok(Bound::Listener(listener)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if let Some((pid, port)) = live_serve(store) {
                return Ok(Bound::AlreadyServed { pid, port });
            }
            tracing::info!("port {default_port} is taken by someone else — asking the OS for a free one");
            Ok(Bound::Listener(bind(0).await.context("could not bind any loopback port")?))
        }
        Err(e) => Err(e).with_context(|| format!("could not bind 127.0.0.1:{default_port}")),
    }
}

async fn bind(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await
}

/// The `{pid, port}` from this store's `serve.pid`, when it names a process that is still alive.
fn live_serve(store: &Store) -> Option<(u32, u16)> {
    let raw = std::fs::read_to_string(store.dir().join("serve.pid")).ok()?;
    let info: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pid = u32::try_from(info["pid"].as_u64()?).ok()?;
    let port = u16::try_from(info["port"].as_u64()?).ok()?;
    pid_alive(pid).then_some((pid, port))
}

/// `kill -0` semantics without unsafe code: ask the `kill` utility whether the process exists. Errs towards "dead"
/// (non-unix, no `kill` binary, someone else's process) — a stale verdict only costs a redundant second server on a
/// fresh port, never a wrong refusal to serve.
fn pid_alive(pid: u32) -> bool {
    cfg!(unix)
        && std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The half of the contract a test may assert without hijacking the developer's browser: `--no-open` reaches no
    /// browser at all. The other half — that both serve paths call this helper — is structural, not observable here.
    #[test]
    fn no_open_opens_nothing() {
        assert!(!open_browser("http://127.0.0.1:4747/", true));
    }
}
