//! Static dashboard assets, embedded into the binary at compile time.
//!
//! The frontend is a self-contained vanilla-ES2022 bundle under
//! `src/dashboard/web/` — no transpile, no minification, no build
//! step. `rust-embed` bakes those files into the binary so the daemon
//! has zero filesystem dependencies at runtime.
//!
//! Routes:
//!
//! - `GET /`            — `index.html`
//! - `GET /static/{path}` — any embedded file by relative path
//!
//! These two routes are intentionally **not** behind the bearer-token
//! middleware. The HTML/JS/CSS payloads are non-sensitive (they hold no
//! data — they only know how to fetch it from `/api/*`); putting them
//! behind auth would prevent the only practical way to bootstrap the
//! token in a browser, since `<script src="?token=...">` does not fly
//! and `EventSource` can't set headers either. The token bootstrap path
//! is documented in the JS: `?token=...` in the URL → localStorage →
//! `Authorization: Bearer …` on every `/api/*` request thereafter. The
//! `/api/*` routes remain auth-gated, which is the actual data
//! boundary.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "src/dashboard/web/"]
struct WebAssets;

/// Build the static asset router. Mount with `.merge()` *outside* the
/// auth layer so HTML/CSS/JS load before the user has stored their
/// token.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", get(index_handler))
        .route("/static/{*path}", get(static_handler))
}

async fn index_handler() -> Response {
    serve_asset("index.html")
}

async fn static_handler(Path(path): Path<String>) -> Response {
    serve_asset(&path)
}

fn serve_asset(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(file) => {
            let mime = mime_for(path);
            ([(header::CONTENT_TYPE, mime)], file.data.into_owned()).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Tiny extension → MIME map. Avoids pulling in the `mime_guess` crate
/// for the four content types we actually serve.
fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_include_index_and_main() {
        // If these blow up the build will too, but assert here so the
        // intent is documented and a missing file fails one focused
        // test instead of a nebulous binary bloat.
        assert!(WebAssets::get("index.html").is_some());
        assert!(WebAssets::get("main.js").is_some());
        assert!(WebAssets::get("style.css").is_some());
    }

    #[test]
    fn mime_table_covers_served_extensions() {
        assert_eq!(mime_for("foo.html"), "text/html; charset=utf-8");
        assert_eq!(mime_for("foo.js"), "application/javascript; charset=utf-8");
        assert_eq!(mime_for("foo.css"), "text/css; charset=utf-8");
        assert_eq!(mime_for("foo.unknown"), "application/octet-stream");
    }
}
