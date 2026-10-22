//! Serves the embedded web UI (web/dist). Unknown non-API paths get index.html
//! so client-side routes work on reload.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::error::AppError;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;

pub async fn serve(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();
    if path == "/api" || path.starts_with("/api/") {
        return AppError::NotFound.into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let rel = path.trim_start_matches('/');
    // Assets are embedded (never read from disk), but refuse odd paths anyway.
    if rel.split('/').any(|seg| seg == ".." || seg == ".")
        || rel.contains('\\')
        || rel.contains('%')
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !rel.is_empty()
        && let Some(file) = Assets::get(rel)
    {
        let immutable = rel.starts_with("assets/");
        return respond(&headers, file, immutable);
    }
    // Paths that look like files but don't exist are real 404s, not SPA routes.
    if rel
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Assets::get("index.html") {
        Some(index) => respond(&headers, index, false),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The web UI is not built. Run `pnpm install && pnpm build` in web/ and rebuild modula-server.",
        )
            .into_response(),
    }
}

fn respond(req: &HeaderMap, file: rust_embed::EmbeddedFile, immutable: bool) -> Response {
    let etag = format!("\"{}\"", hex_prefix(&file.metadata.sha256_hash()));
    let cache = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut res =
        if req.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            let mut r = Response::new(Body::from(file.data.into_owned()));
            if let Ok(ct) = HeaderValue::from_str(file.metadata.mimetype()) {
                r.headers_mut().insert(header::CONTENT_TYPE, ct);
            }
            r
        };
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    res
}

fn hex_prefix(hash: &[u8; 32]) -> String {
    hash[..12].iter().map(|b| format!("{b:02x}")).collect()
}
