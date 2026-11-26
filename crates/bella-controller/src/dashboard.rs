//! The embedded dashboard: `index.html` at `/` and its static files.
//!
//! The files are compiled into the binary by the build script when
//! `dashboard/dist` exists. Nothing is read from disk at run time, so a
//! request can only ever name one of the embedded files: there is no path
//! to traverse. The dashboard routes by URL fragment, so no other path
//! falls back to `index.html`.

use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use axum::http::{HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

include!(concat!(env!("OUT_DIR"), "/dashboard_assets.rs"));

/// The page may load scripts, styles, images and fonts from this origin and
/// talk to this origin's API. Nothing inline, nothing from elsewhere, no
/// framing, no form posts.
pub(crate) const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'none'; script-src 'self'; \
    style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; \
    manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const PLACEHOLDER: &str = "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>Builderella</title>\n</head>\n<body>\n<h1>Builderella controller</h1>\n\
<p>The dashboard was not built into this binary.</p>\n\
<p>Build it with <code>npm ci &amp;&amp; npm run build</code> in <code>dashboard/</code>, \
then rebuild the controller. The API under <code>/api/v1</code> and the <code>bella</code> \
command line work without it.</p>\n</body>\n</html>\n";

/// Whether a built dashboard is embedded in this binary.
pub(crate) fn is_embedded() -> bool {
    find("index.html").is_some()
}

fn find(path: &str) -> Option<&'static [u8]> {
    ASSETS
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, bytes)| *bytes)
}

fn content_type(path: &str) -> &'static str {
    let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
    match extension.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn respond(
    status: StatusCode,
    media_type: &'static str,
    cache: &'static str,
    body: &'static [u8],
) -> Response {
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(media_type));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY_VALUE),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    response
}

fn not_found() -> Response {
    respond(
        StatusCode::NOT_FOUND,
        "text/plain; charset=utf-8",
        "no-store",
        b"not found\n",
    )
}

/// `GET /`: the dashboard, or a page saying it was not built in. Never
/// cached, so a new controller version is picked up on the next load.
pub(crate) async fn index() -> Response {
    let body = find("index.html").unwrap_or(PLACEHOLDER.as_bytes());
    respond(StatusCode::OK, "text/html; charset=utf-8", "no-cache", body)
}

/// A request path as the name of an embedded file, if it can be one. Paths
/// are matched literally: anything encoded, relative or oddly separated is
/// not a file the build produced.
fn asset_name(path: &str) -> Option<&str> {
    let name = path.strip_prefix('/')?;
    let plain = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'@' | b'~'));
    let clean = name
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    (plain && clean).then_some(name)
}

/// Every other path: one of the embedded files, or 404. Files under
/// `assets/` carry a content hash in their name and are cached for good.
pub(crate) async fn asset(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return not_found();
    }
    let Some(name) = asset_name(uri.path()) else {
        return not_found();
    };
    match find(name) {
        Some(body) => {
            let cache = if name.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            respond(StatusCode::OK, content_type(name), cache, body)
        }
        None => not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_relative_names_can_be_assets() {
        assert_eq!(
            asset_name("/assets/index-Ab3_x.js"),
            Some("assets/index-Ab3_x.js")
        );
        assert_eq!(asset_name("/favicon.svg"), Some("favicon.svg"));
        for hostile in [
            "/../etc/passwd",
            "/assets/../../secret",
            "/assets/%2e%2e/secret",
            "/assets//x.js",
            "/assets/./x.js",
            "/assets\\x.js",
            "/",
            "",
            "assets/x.js",
            "/assets/x.js?y",
            "/c:/windows",
        ] {
            assert_eq!(asset_name(hostile), None, "{hostile}");
        }
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(
            content_type("assets/a.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("assets/a.CSS"), "text/css; charset=utf-8");
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("blob"), "application/octet-stream");
    }

    #[test]
    fn the_placeholder_needs_nothing_the_policy_forbids() {
        assert!(!PLACEHOLDER.contains("<script"));
        assert!(!PLACEHOLDER.contains("<style"));
        assert!(!PLACEHOLDER.contains("style="));
        assert!(!PLACEHOLDER.contains("http://"));
        assert!(!PLACEHOLDER.contains("https://"));
    }

    #[tokio::test]
    async fn pages_carry_the_security_headers() {
        let page = index().await;
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(page.headers()[CACHE_CONTROL], "no-cache");
        assert_eq!(
            page.headers()[CONTENT_SECURITY_POLICY],
            CONTENT_SECURITY_POLICY_VALUE
        );
        let missing = asset(Method::GET, Uri::from_static("/assets/nope.js")).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(missing.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
        let post = asset(Method::POST, Uri::from_static("/favicon.svg")).await;
        assert_eq!(post.status(), StatusCode::NOT_FOUND);
    }
}
