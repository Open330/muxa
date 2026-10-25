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
//! token in a browser, since `EventSource` can't set headers either. The
//! token bootstrap path is documented in the JS: `#token=...` in the URL
//! *fragment* (which never reaches the server) → localStorage →
//! `Authorization: Bearer …` on every `/api/*` request thereafter. The
//! `/api/*` routes remain auth-gated, which is the actual data
//! boundary. They are additionally wrapped by a DNS-rebinding `Host`
//! guard in `server.rs`.

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
    serve_page("index.html")
}

async fn static_handler(Path(path): Path<String>) -> Response {
    serve_asset(unversioned(&path))
}

/// `no-cache` asks every cache to revalidate with the daemon. It is not
/// enough on its own: a CDN in front of muxad can rewrite it (Cloudflare's
/// Browser Cache TTL turns it into `max-age=14400`), so browsers kept the
/// previous build's `main.js` next to the new `index.html` for hours after
/// an upgrade. Pages therefore also link assets under a build-specific
/// path (`serve_page`).
const ASSET_CACHE_CONTROL: &str = "no-cache";

/// Identifies this build's embedded web assets: a digest of every file's
/// content hash, so it changes exactly when any asset does.
fn asset_build_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for path in WebAssets::iter() {
            path.hash(&mut hasher);
            if let Some(file) = WebAssets::get(&path) {
                file.metadata.sha256_hash().hash(&mut hasher);
            }
        }
        format!("{:012x}", hasher.finish() & 0xffff_ffff_ffff)
    })
}

/// `/static/v/<build>/main.js` is `/static/main.js`. Any build id is
/// accepted: a page cached from an older build still loads (current) files.
fn unversioned(path: &str) -> &str {
    path.strip_prefix("v/")
        .and_then(|rest| rest.split_once('/'))
        .map_or(path, |(_, file)| file)
}

/// Serve an HTML page with its `/static/` links moved under this build's
/// path. Relative module imports inside those scripts resolve under the
/// same prefix, so one new build id changes every asset URL the page uses.
pub(super) fn serve_page(path: &str) -> Response {
    let Some(file) = WebAssets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let html = String::from_utf8_lossy(&file.data)
        .replace("/static/", &format!("/static/v/{}/", asset_build_id()));
    (
        [
            (header::CONTENT_TYPE, mime_for(path)),
            (header::CACHE_CONTROL, ASSET_CACHE_CONTROL),
        ],
        html,
    )
        .into_response()
}

pub(super) fn serve_asset(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(file) => {
            let mime = mime_for(path);
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, ASSET_CACHE_CONTROL),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Tiny extension → MIME map. Avoids pulling in the `mime_guess` crate
/// for the handful of content types we actually serve.
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
        "webp" => "image/webp",
        "jpg" | "jpeg" => "image/jpeg",
        "woff2" => "font/woff2",
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
        assert!(WebAssets::get("work-model.mjs").is_some());
        assert!(WebAssets::get("collaboration-model.mjs").is_some());
        assert!(WebAssets::get("style.css").is_some());
        assert!(WebAssets::get("operator-enroll.mjs").is_some());
        assert!(WebAssets::get("operators-admin.mjs").is_some());
        assert!(WebAssets::get("pane-drawer.mjs").is_some());
        assert!(WebAssets::get("pane-stream.mjs").is_some());
        assert!(WebAssets::get("topology-model.mjs").is_some());
        assert!(WebAssets::get("workspace-rail.mjs").is_some());
        assert!(WebAssets::get("landing.mjs").is_some());
        assert!(WebAssets::get("landing-film.mjs").is_some());
        assert!(WebAssets::get("landing-frames.mjs").is_some());
        for mac in ["mac-icon", "mac-watch", "mac-inbox"] {
            assert!(
                WebAssets::get(&format!("landing/{mac}.webp")).is_some(),
                "missing {mac}"
            );
        }
        assert!(WebAssets::get("landing.css").is_some());
        assert!(WebAssets::get("fonts/LICENSE-nerd-fonts-symbols.txt").is_some());
        for shot in ["board", "collaboration", "agents"] {
            for scheme in ["light", "dark"] {
                let path = format!("landing/{shot}-{scheme}.webp");
                assert!(WebAssets::get(&path).is_some(), "missing {path}");
            }
        }
    }

    #[test]
    fn assets_tell_caches_to_revalidate() {
        for path in [
            "index.html",
            "main.js",
            "style.css",
            "landing/board-light.webp",
        ] {
            let response = serve_asset(path);
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-cache",
                "{path}"
            );
        }
    }

    #[test]
    fn nerd_font_symbols_are_embedded_and_served_as_woff2() {
        let path = "fonts/muxa-nerd-symbols.woff2";
        let file = WebAssets::get(path).expect("bundled Nerd Font symbols");
        assert_eq!(&file.data[..4], b"wOF2", "not a woff2 file");
        let response = serve_asset(path);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "font/woff2");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        // Every stylesheet that renders pane output falls back to it.
        for css in ["style.css", "share.css"] {
            let text = String::from_utf8(WebAssets::get(css).unwrap().data.into_owned()).unwrap();
            assert!(
                text.contains("url(\"/static/fonts/muxa-nerd-symbols.woff2\")"),
                "{css}"
            );
            assert!(
                text.contains("unicode-range: U+E000-F8FF, U+F0000-FFFFF"),
                "{css}"
            );
        }
    }

    #[test]
    fn pages_link_assets_under_this_builds_path_and_those_paths_resolve() {
        let id = asset_build_id();
        assert_eq!(id.len(), 12);
        assert_eq!(id, asset_build_id(), "stable within a build");
        assert_eq!(unversioned(&format!("v/{id}/main.js")), "main.js");
        assert_eq!(
            unversioned("v/older/landing/board-dark.webp"),
            "landing/board-dark.webp"
        );
        assert_eq!(unversioned("main.js"), "main.js");
        assert_eq!(unversioned("v"), "v");
        let versioned = serve_asset(unversioned(&format!("v/{id}/main.js")));
        assert_eq!(versioned.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn index_html_points_every_static_link_at_the_build_path() {
        use http_body_util::BodyExt as _;
        let response = serve_page("index.html");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(body.to_vec()).unwrap();
        let prefix = format!("/static/v/{}/", asset_build_id());
        assert!(html.contains(&format!("{prefix}main.js")));
        assert!(html.contains(&format!("{prefix}style.css")));
        assert!(!html.replace(&prefix, "").contains("/static/"));
    }

    #[test]
    fn mime_table_covers_served_extensions() {
        assert_eq!(mime_for("foo.html"), "text/html; charset=utf-8");
        assert_eq!(mime_for("foo.js"), "application/javascript; charset=utf-8");
        assert_eq!(mime_for("foo.mjs"), "application/javascript; charset=utf-8");
        assert_eq!(mime_for("foo.css"), "text/css; charset=utf-8");
        assert_eq!(mime_for("landing/shot.webp"), "image/webp");
        assert_eq!(mime_for("foo.jpg"), "image/jpeg");
        assert_eq!(mime_for("fonts/foo.woff2"), "font/woff2");
        assert_eq!(mime_for("foo.unknown"), "application/octet-stream");
    }
}
