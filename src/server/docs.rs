//! In-app documentation viewer: a small TOC over the top-level markdown files in `docs/`, embedded into the binary at
//! build time (`rust-embed`) so `cargo build` alone continues to yield a self-contained plugin. The renderer is the
//! existing client-side pipeline: article fragments carry `data-md-src`, glue.js fetches the raw markdown and pipes it
//! through marked + `DOMPurify` + highlight.js — the same pipeline ticket and epic bodies use.
//!
//! Two URL prefixes, deliberately split so they cannot collide: `/docs/page/{name}` for article fragments and
//! `/docs/assets/{*path}` for images the docs reference. Filenames off the URL are validated in every handler (no `/`,
//! no `\`, no `..`, no leading `.`); `DocsEmbed::get()` is a second layer that only serves keys known at build time.

use std::{borrow::Cow, path::PathBuf};

use axum::{
    extract::Path as UrlPath,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use rust_embed::RustEmbed;

use super::{routes::AppError, views};

/// Everything under `docs/` at build time: top-level `*.md` files (the TOC entries) and anything the docs reference
/// under `assets/`. A missing `docs/` folder is a compile error — the seed files under version control are the safety
/// net for that.
#[derive(RustEmbed)]
#[folder = "docs/"]
struct DocsEmbed;

/// One entry in the TOC: the filename (used in URLs) and the human title (the file's first `# H1`, or a humanised
/// fallback).
#[derive(Debug, Clone)]
pub struct DocEntry {
    pub file: String,
    pub title: String,
}

/// The TOC — the top-level `*.md` files, alphabetically. Anything under `assets/` is skipped: assets aren't docs.
#[must_use]
pub fn list_docs() -> Vec<DocEntry> {
    let mut entries: Vec<DocEntry> = DocsEmbed::iter()
        .filter(|f| is_top_level_markdown(f))
        .filter_map(|f| {
            let bytes = DocsEmbed::get(&f)?.data.into_owned();
            let title = derive_title(&bytes, &f);
            Some(DocEntry { file: f.into_owned(), title })
        })
        .collect();
    entries.sort_by(|a, b| a.file.cmp(&b.file));
    entries
}

fn is_top_level_markdown(path: &str) -> bool {
    !path.contains('/') && std::path::Path::new(path).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// The article's human title: the first `# H1` in the file, falling back to a humanised filename when the file has no
/// leading heading. Kept plain-old-bytes so the parser is testable without the embed macro.
fn derive_title(bytes: &[u8], filename: &str) -> String {
    let text = std::str::from_utf8(bytes).unwrap_or("");
    let heading = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| line.strip_prefix("# "))
        .map(str::trim);
    match heading {
        Some(h) if !h.is_empty() => h.to_owned(),
        _ => humanise_filename(filename),
    }
}

/// `getting-started.md` → `Getting started`. Dashes/underscores become spaces, the `.md` goes, and the first character
/// is uppercased — no aggressive title-casing (`In-App` for `in-app`) because the fallback only runs on docs their
/// author forgot to give an H1, and preserving user casing is friendlier than mangling it.
fn humanise_filename(filename: &str) -> String {
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    let spaced: String = stem.chars().map(|c| if c == '-' || c == '_' { ' ' } else { c }).collect();
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Guard every filename that arrived off a URL: reject traversal shapes and hidden files. `DocsEmbed::get()` gives us
/// build-time safety too, but keeping the check local means the invariant is visible in each handler.
fn safe_filename(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains("..") && !name.starts_with('.')
}

/// A path underneath `docs/assets/` — same rules as [`safe_filename`], applied per segment.
fn safe_asset_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('\\') && path.split('/').all(safe_filename)
}

// ---- handlers -------------------------------------------------------------------------------------------------------

/// `GET /docs` — the modal shell: the TOC on the left, the first entry's article primed on the right for glue.js to
/// fetch and render.
pub async fn shell() -> Result<Html<String>, AppError> {
    use askama::Template;
    let tpl = views::docs(list_docs());
    Ok(Html(tpl.render()?))
}

/// `GET /docs/page/{name}` — the article fragment for one doc, ready to be swapped into `#docs-content`. The fragment
/// is one line of HTML carrying `data-md-src`; glue.js does the fetch and the render.
pub async fn page(UrlPath(name): UrlPath<String>) -> Result<Html<String>, AppError> {
    if !safe_filename(&name) || DocsEmbed::get(&name).is_none() {
        return Err(AppError::not_found("doc"));
    }
    // `name` is filtered by `safe_filename` and confirmed to be a real doc key — traversal characters are impossible,
    // so the `data-md-src` attribute is safe to interpolate verbatim.
    Ok(Html(format!(
        r#"<article class="prose prose-sm max-w-none" data-md-src="/raw/docs/{name}"></article>"#
    )))
}

/// `GET /raw/docs/{name}` — the raw markdown bytes for the client-side renderer. Same shape as `raw_ticket` /
/// `raw_epic`.
pub async fn raw(UrlPath(name): UrlPath<String>) -> Result<Response, AppError> {
    if !safe_filename(&name) {
        return Err(AppError::not_found("doc"));
    }
    let file = DocsEmbed::get(&name).ok_or_else(|| AppError::not_found("doc"))?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], file.data.into_owned()).into_response())
}

/// `GET /docs/assets/{*path}` — images (etc.) referenced from doc bodies. No `--assets-dir` override: docs are shipped
/// content, not dev scaffolding.
pub async fn asset(UrlPath(path): UrlPath<String>) -> Response {
    if !safe_asset_path(&path) {
        return (StatusCode::NOT_FOUND, "no such doc asset").into_response();
    }
    let key = PathBuf::from("assets").join(&path);
    let key = key.to_string_lossy().replace('\\', "/");
    let body: Option<Cow<'static, [u8]>> = DocsEmbed::get(&key).map(|f| f.data);
    match body {
        Some(data) => ([(header::CONTENT_TYPE, content_type(&path))], data).into_response(),
        None => (StatusCode::NOT_FOUND, "no such doc asset").into_response(),
    }
}

/// Duplicates the four-line matcher from [`super::assets::content_type`] rather than sharing it: the two modules stay
/// independent, and the list is short enough that centralising costs more than it saves.
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_title_uses_first_h1_when_present() {
        assert_eq!(derive_title(b"# Hello\n\nrest", "any.md"), "Hello");
    }

    #[test]
    fn derive_title_skips_leading_blank_lines() {
        assert_eq!(derive_title(b"\n\n# Hi\n\nrest", "any.md"), "Hi");
    }

    #[test]
    fn derive_title_falls_back_to_filename_when_no_h1() {
        assert_eq!(derive_title(b"no heading here", "getting-started.md"), "Getting started");
    }

    #[test]
    fn derive_title_ignores_non_h1_lines() {
        assert_eq!(derive_title(b"Not a heading\n# Later", "workflow.md"), "Workflow");
    }

    #[test]
    fn safe_filename_rejects_traversal() {
        assert!(safe_filename("getting-started.md"));
        assert!(!safe_filename("../Cargo.toml"));
        assert!(!safe_filename("foo/bar"));
        assert!(!safe_filename(".hidden"));
        assert!(!safe_filename(""));
        assert!(!safe_filename("a\\b"));
    }

    #[test]
    fn safe_asset_path_walks_each_segment() {
        assert!(safe_asset_path("logo.svg"));
        assert!(safe_asset_path("images/logo.svg"));
        assert!(!safe_asset_path("../Cargo.toml"));
        assert!(!safe_asset_path("images/../secret"));
        assert!(!safe_asset_path(""));
    }
}
