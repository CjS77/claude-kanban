//! Hardening for a loopback HTTP server. The threat is not the network — the listener binds `127.0.0.1` only — it is the
//! user's own browser running someone else's page. Two attacks matter, and one middleware kills both:
//!
//! - **DNS rebinding**: a malicious page at `evil.example` re-points its DNS at `127.0.0.1` and reads our responses as if
//!   same-origin. Its requests necessarily carry `Host: evil.example`, so an exact Host allowlist stops every one of them,
//!   reads included.
//! - **CSRF**: a cross-site form POSTs to `http://127.0.0.1:4747/ui/...`. Cross-site forms cannot set custom headers, and a
//!   cross-origin `fetch` with one triggers a CORS preflight this server never grants (it sends no `Access-Control-Allow-*`
//!   at all — the absence is the feature). So requiring the `X-Board-Version` header on every non-GET — a header our own
//!   UI already sends for optimistic concurrency — makes mutations unforgeable. A present-but-wrong `Origin` is rejected
//!   as belt-and-braces.
//!
//! There are no cookies and no auth state, so there is nothing else for a cross-site request to ride on.

use axum::{
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::AppState;

/// The custom header carrying the board version on mutations — doubling as the CSRF token (see module docs).
pub const VERSION_HEADER: &str = "x-board-version";

/// The one gate every request passes: exact Host allowlist for all methods, custom-header + Origin checks for mutations.
pub async fn guard(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|host| app.allowed_hosts.iter().any(|allowed| allowed == host));
    if !host_ok {
        return refuse("this board only answers to localhost");
    }

    if req.method() != Method::GET && req.method() != Method::HEAD {
        if !req.headers().contains_key(VERSION_HEADER) {
            return refuse("mutations need the X-Board-Version header (are you using the board UI?)");
        }
        let origin_ok = match req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
            None => true, // same-origin non-CORS requests may omit it; the custom header already gates these
            Some(origin) => app.allowed_origins.iter().any(|allowed| allowed == origin),
        };
        if !origin_ok {
            return refuse("cross-origin mutations are not allowed");
        }
    }

    next.run(req).await
}

fn refuse(msg: &'static str) -> Response {
    (StatusCode::FORBIDDEN, msg).into_response()
}
