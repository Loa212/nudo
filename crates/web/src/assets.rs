//! Vendored front-end assets, embedded in the binary.
//!
//! Serving these from the binary rather than a CDN means the dashboard renders
//! with no network egress, there is no JavaScript build step, and a control plane
//! that manages production hosts does not fetch executable code from a third
//! party at page load.

use axum::http::header;
use axum::response::{IntoResponse, Response};

/// One embedded asset.
struct Asset {
    bytes: &'static [u8],
    content_type: &'static str,
}

const HTMX: Asset = Asset {
    bytes: include_bytes!("assets/htmx.min.js"),
    content_type: "application/javascript; charset=utf-8",
};

const SSE: Asset = Asset {
    bytes: include_bytes!("assets/sse.js"),
    content_type: "application/javascript; charset=utf-8",
};

const XTERM_JS: Asset = Asset {
    bytes: include_bytes!("assets/xterm.js"),
    content_type: "application/javascript; charset=utf-8",
};

const XTERM_FIT: Asset = Asset {
    bytes: include_bytes!("assets/xterm-addon-fit.js"),
    content_type: "application/javascript; charset=utf-8",
};

const XTERM_CSS: Asset = Asset {
    bytes: include_bytes!("assets/xterm.css"),
    content_type: "text/css; charset=utf-8",
};

const APP_CSS: Asset = Asset {
    bytes: include_bytes!("assets/app.css"),
    content_type: "text/css; charset=utf-8",
};

const TERMINAL_JS: Asset = Asset {
    bytes: include_bytes!("assets/terminal.js"),
    content_type: "application/javascript; charset=utf-8",
};

/// Serves an asset by name.
///
/// A fixed match rather than a path lookup, so there is no way to traverse out of
/// the asset set — the router never touches the filesystem.
pub async fn serve(name: &str) -> Response {
    let asset = match name {
        "htmx.min.js" => HTMX,
        "sse.js" => SSE,
        "xterm.js" => XTERM_JS,
        "xterm-addon-fit.js" => XTERM_FIT,
        "xterm.css" => XTERM_CSS,
        "app.css" => APP_CSS,
        "terminal.js" => TERMINAL_JS,
        _ => return axum::http::StatusCode::NOT_FOUND.into_response(),
    };

    (
        [
            (header::CONTENT_TYPE, asset.content_type),
            // Cached hard, and safely: every reference carries `?v=<build>`, so
            // a new binary asks for a URL the browser has never seen. Without
            // that, this header meant an hour of stale CSS after every deploy
            // — invisible to whoever shipped it, since their own reload was
            // usually a hard one.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        asset.bytes,
    )
        .into_response()
}

/// A fingerprint of the embedded assets, appended to every asset URL.
///
/// Derived from the bytes themselves rather than a version number or a build
/// timestamp: a rebuild that changes nothing keeps the same URL and the cache
/// stays warm, while any real change to an asset produces a different one and
/// invalidates it. The crate version would not do — the dashboard is rebuilt
/// far more often than it is released, which is exactly when stale CSS bites.
pub fn build_id() -> &'static str {
    use std::sync::OnceLock;

    static BUILD_ID: OnceLock<String> = OnceLock::new();
    BUILD_ID.get_or_init(|| {
        // FNV-1a over each asset's bytes. Not cryptographic — this only has to
        // differ when the content differs, and be cheap to compute once.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for asset in [
            APP_CSS,
            TERMINAL_JS,
            HTMX,
            SSE,
            XTERM_JS,
            XTERM_FIT,
            XTERM_CSS,
        ] {
            for byte in asset.bytes {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        format!("{hash:x}")
    })
}

/// The URL for an embedded asset, fingerprinted so a stale copy is never used.
pub fn url(name: &str) -> String {
    format!("/assets/{name}?v={}", build_id())
}

/// The names this module will serve. Used by tests and by the page templates so
/// a typo in a `<script src>` is caught rather than 404ing at runtime.
pub const ASSET_NAMES: [&str; 7] = [
    "htmx.min.js",
    "sse.js",
    "xterm.js",
    "xterm-addon-fit.js",
    "xterm.css",
    "app.css",
    "terminal.js",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_declared_asset_is_served_with_a_body_and_a_content_type() {
        for name in ASSET_NAMES {
            let response = serve(name).await;
            assert_eq!(
                response.status(),
                axum::http::StatusCode::OK,
                "{name} was not served"
            );

            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if name.ends_with(".css") {
                assert!(
                    content_type.starts_with("text/css"),
                    "{name}: {content_type}"
                );
            } else {
                assert!(
                    content_type.starts_with("application/javascript"),
                    "{name}: {content_type}"
                );
            }
        }
    }

    #[tokio::test]
    async fn an_unknown_asset_is_a_not_found_rather_than_a_filesystem_read() {
        for name in ["nope.js", "../../etc/passwd", "", "app.css/../secret"] {
            let response = serve(name).await;
            assert_eq!(
                response.status(),
                axum::http::StatusCode::NOT_FOUND,
                "{name:?} should not resolve"
            );
        }
    }

    #[test]
    fn the_embedded_assets_are_not_empty() {
        // A truncated vendored file would produce a blank page with no error.
        assert!(HTMX.bytes.len() > 10_000, "htmx looks truncated");
        assert!(XTERM_JS.bytes.len() > 100_000, "xterm looks truncated");
        assert!(APP_CSS.bytes.len() > 1_000, "app.css looks truncated");
        assert!(TERMINAL_JS.bytes.len() > 500, "terminal.js looks truncated");
        assert!(SSE.bytes.len() > 1_000, "the sse extension looks truncated");
        assert!(XTERM_CSS.bytes.len() > 500, "xterm.css looks truncated");
        assert!(XTERM_FIT.bytes.len() > 200, "the fit addon looks truncated");
    }

    #[test]
    fn the_vendored_javascript_is_what_it_claims_to_be() {
        // Guards against a failed download leaving an HTML error page in place
        // of a script.
        let htmx = String::from_utf8_lossy(HTMX.bytes);
        assert!(htmx.contains("htmx"), "htmx.min.js does not look like htmx");

        let xterm = String::from_utf8_lossy(XTERM_JS.bytes);
        assert!(
            xterm.contains("Terminal"),
            "xterm.js does not define Terminal"
        );

        let fit = String::from_utf8_lossy(XTERM_FIT.bytes);
        assert!(
            fit.contains("FitAddon"),
            "the fit addon does not define FitAddon"
        );
    }
}
