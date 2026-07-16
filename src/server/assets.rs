//! Web assets, baked into the binary at compile time so `target/release/claude-kanban` is self-contained wherever it lands.
//!
//! `serve --assets-dir <dir>` overrides with a live directory for UI development (edit, refresh, no rebuild). Everything is
//! served `Cache-Control: no-cache` — the files are tiny, the server is loopback, and a stale UI after a rebuild would cost
//! more than the revalidation ever will.

use std::{borrow::Cow, path::Path};

use axum::{
    extract::{Path as UrlPath, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

use super::AppState;

/// Everything under `assets/` at build time: vendored htmx/SortableJS/marked/DOMPurify, the generated app.css, and glue.js.
#[derive(RustEmbed)]
#[folder = "assets/"]
struct Embedded;

/// `GET /assets/{*path}` — embedded bytes, or the `--assets-dir` override when set.
pub async fn asset(State(app): State<AppState>, UrlPath(path): UrlPath<String>) -> Response {
    let body: Option<Cow<'static, [u8]>> = match &app.assets_dir {
        Some(dir) => read_from_disk(dir, &path).map(Cow::Owned),
        None => Embedded::get(&path).map(|f| f.data),
    };
    match body {
        Some(data) => ([(header::CONTENT_TYPE, content_type(&path)), (header::CACHE_CONTROL, "no-cache")], data).into_response(),
        None => (StatusCode::NOT_FOUND, "no such asset").into_response(),
    }
}

/// Dev-mode disk read, pinned inside the assets dir — a traversal like `../Cargo.toml` resolves outside it and is refused.
fn read_from_disk(dir: &Path, path: &str) -> Option<Vec<u8>> {
    let root = dir.canonicalize().ok()?;
    let file = root.join(path).canonicalize().ok()?;
    file.starts_with(&root).then(|| std::fs::read(file).ok()).flatten()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("map" | "json") => "application/json",
        _ => "application/octet-stream",
    }
}
