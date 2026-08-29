#[cfg(feature = "embed-ui")]
use axum::body::Body;
use axum::{
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
#[cfg(feature = "embed-ui")]
use sha2::{Digest, Sha256};

#[cfg(feature = "embed-ui")]
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

#[cfg(feature = "embed-ui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist/"]
struct WebAssets;

pub async fn serve(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let raw = uri.path();
    if raw == "/healthz" || raw == "/api" || raw.starts_with("/api/") {
        return api_not_found();
    }
    if unsafe_path(raw) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    #[cfg(not(feature = "embed-ui"))]
    {
        let _ = headers;
        StatusCode::NOT_FOUND.into_response()
    }
    #[cfg(feature = "embed-ui")]
    {
        let requested = raw.trim_start_matches('/');
        let (name, cache) = if requested.is_empty() {
            ("index.html", "no-cache")
        } else if requested.starts_with("assets/") {
            (requested, "public, max-age=31536000, immutable")
        } else if !requested
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .contains('.')
            && accepts_html(&headers)
        {
            ("index.html", "no-cache")
        } else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let Some(asset) = WebAssets::get(name) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if name.ends_with(".map") || name.contains(".env") {
            return StatusCode::NOT_FOUND.into_response();
        }
        let bytes = asset.data.into_owned();
        let etag = format!("\"sha256-{:x}\"", Sha256::digest(&bytes));
        let not_modified = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.split(',').any(|value| value.trim() == etag));
        let status = if not_modified {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        };
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type(name))
            .header(header::CACHE_CONTROL, cache)
            .header(header::ETAG, etag)
            .header("x-content-type-options", "nosniff")
            .header("referrer-policy", "no-referrer")
            .header("x-frame-options", "DENY")
            .header(
                "permissions-policy",
                "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
            )
            .header("content-security-policy", CSP)
            .body(if method == Method::HEAD || not_modified {
                Body::empty()
            } else {
                Body::from(bytes)
            })
            .expect("static response headers are valid");
        if not_modified {
            response.headers_mut().remove(header::CONTENT_LENGTH);
        }
        response
    }
}

fn unsafe_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    path.contains("//")
        || path.contains('\0')
        || lower.contains("%00")
        || lower.contains("%2f")
        || lower.contains("%5c")
        || lower.contains("%2e")
        || path.split('/').any(|part| matches!(part, "." | ".."))
}

#[cfg(feature = "embed-ui")]
fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().starts_with("text/html"))
        })
}

#[cfg(feature = "embed-ui")]
fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        r#"{"code":"NOT_FOUND","message":"The API endpoint was not found"}"#,
    )
        .into_response()
}

#[cfg(all(test, feature = "embed-ui"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn index_and_spa_have_security_headers_but_api_never_falls_back() {
        let index = serve(Method::GET, "/".parse().unwrap(), HeaderMap::new()).await;
        assert_eq!(index.status(), StatusCode::OK);
        assert_eq!(index.headers()[header::CACHE_CONTROL], "no-cache");
        assert!(
            index.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("object-src 'none'")
        );
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("text/html"));
        assert_eq!(
            serve(Method::GET, "/apps/example".parse().unwrap(), headers)
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            serve(
                Method::GET,
                "/api/v1/missing".parse().unwrap(),
                HeaderMap::new()
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn head_etag_and_traversal_are_bounded() {
        let first = serve(Method::GET, "/".parse().unwrap(), HeaderMap::new()).await;
        let etag = first.headers()[header::ETAG].clone();
        let mut conditional = HeaderMap::new();
        conditional.insert(header::IF_NONE_MATCH, etag);
        assert_eq!(
            serve(Method::GET, "/".parse().unwrap(), conditional)
                .await
                .status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            serve(Method::HEAD, "/".parse().unwrap(), HeaderMap::new())
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            serve(
                Method::GET,
                "/assets/%2fetc".parse().unwrap(),
                HeaderMap::new()
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
