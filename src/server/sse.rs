//! Live updates: one watcher, one broadcast channel, one tiny event.
//!
//! The serve process never signals its own writes in-process — every refresh, whatever its origin (a browser action, the
//! MCP process, a hand edit in vim), starts as an observed change to a file in `.kanban/`. One path, no special cases, and
//! the two processes stay coupled only through the store, as designed.
//!
//! ```text
//! notify watcher on the .kanban DIRECTORY      (a rename swaps inodes, so watching the file itself would go blind
//!    │  filter: board.json / claims.json        after the first atomic write)
//!    ▼  mpsc (sync send from notify's thread)
//! debounce task: first event → 100ms drain → read version → broadcast
//!    ▼  broadcast<u64>
//! GET /events: `board-changed` + version, keep-alive every 15s, closed by the shutdown token
//! ```
//!
//! The event body is just the version: the browser refetches the board fragment, which is the *one* rendering path.
//! Claims-only writes broadcast too — claims render on cards, and version dedup happens client-side.

use std::time::Duration;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::StreamExt;
use notify::{RecursiveMode, Watcher};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use super::AppState;
use crate::store::Store;

/// How long to keep draining filesystem events before reading the board: one logical write is several notify events
/// (create temp, rename), and editors add their own noise.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// Start the notify watcher and its debounce task; returns the channel SSE handlers subscribe to.
pub fn spawn_watcher(store: Store, shutdown: CancellationToken) -> anyhow::Result<broadcast::Sender<u64>> {
    let (tx, _) = broadcast::channel(16);
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(event) = res {
            let interesting = event
                .paths
                .iter()
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .any(|name| name == "board.json" || name == "claims.json");
            if interesting {
                let _ = raw_tx.send(());
            }
        }
    })?;
    watcher.watch(store.dir(), RecursiveMode::NonRecursive)?;

    let sender = tx.clone();
    tokio::spawn(async move {
        let _watcher = watcher; // owned here: dropping it would stop the events
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                first = raw_rx.recv() => {
                    if first.is_none() {
                        break;
                    }
                }
            }
            tokio::time::sleep(DEBOUNCE).await;
            while raw_rx.try_recv().is_ok() {}
            // Version 0 on a read failure is fine: it never equals the browser's current version, so it still refreshes.
            let version = store.read_board().map_or(0, |b| b.version);
            tracing::debug!(version, subscribers = sender.receiver_count(), "store changed — broadcasting refresh");
            let _ = sender.send(version);
        }
    });

    Ok(tx)
}

/// `GET /events` — the SSE stream. Ends when the shutdown token trips (open `EventSource` connections would otherwise hold
/// graceful shutdown hostage forever); the browser's `EventSource` reconnects on its own when the server comes back.
pub async fn events(State(app): State<AppState>) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = BroadcastStream::new(app.refresh.subscribe())
        .filter_map(|item| async move {
            match item {
                Ok(version) => Some(Ok(Event::default().event("board-changed").data(version.to_string()))),
                // Lagged: we missed some broadcasts. Only the latest matters and another is coming — skip.
                Err(_) => None,
            }
        })
        .take_until(app.shutdown.clone().cancelled_owned());
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
