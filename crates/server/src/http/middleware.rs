use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::tokens;
use crate::error::AppError;
use crate::state::AppState;

/// Endpoints that authenticate by a signature of their own instead of cookies
/// or API tokens (no Origin check): GitHub's webhook, and nodes uploading
/// build output with a deployment-bound token (`deploy::output`).
const SIGNED: &[(Method, &str)] = &[
    (Method::POST, crate::api::github::WEBHOOK_PATH),
    (Method::PUT, crate::deploy::output::UPLOAD_PATH),
];

/// The CLI's browser login (`api::cli_login`), called without cookies or
/// Origin: starting one grants nothing, and collecting the token needs the
/// secret device code from the body, which a cross-site page doesn't have.
const CLI_LOGIN: &[(Method, &str)] = &[
    (Method::POST, crate::api::cli_login::START_PATH),
    (Method::POST, crate::api::cli_login::TOKEN_PATH),
];

/// CSRF defence: every state-changing request must come from our exact origin.
/// Browsers always send `Origin` on cross-origin and same-origin non-GET fetches,
/// so a missing header on a mutation is rejected too.
///
/// Requests carrying `Authorization: Bearer <API token>` (the CLI sends no
/// Origin) are exempt, but only once the token itself checks out: it is
/// resolved here and handed to the route guard, which then authenticates by
/// that token alone and never by cookies. A forged cross-site request can't
/// carry the token, so the exemption can't be ridden with a victim's cookie.
pub async fn require_same_origin(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let signed = SIGNED
        .iter()
        .chain(CLI_LOGIN)
        .any(|(m, p)| req.method() == m && req.uri().path() == *p);
    if signed {
        return next.run(req).await;
    }
    // Safe methods need no exemption; the route guard checks their token.
    if let Some(presented) = tokens::bearer(req.headers()).filter(|_| !safe) {
        let actor = match presented {
            Ok(t) => tokens::resolve(&state.db, t).await,
            Err(e) => Err(e),
        };
        return match actor {
            Ok(Some(actor)) => {
                req.extensions_mut().insert(tokens::BearerActor(actor));
                next.run(req).await
            }
            Ok(None) => AppError::Unauthorized.into_response(),
            Err(e) => e.into_response(),
        };
    }
    if safe {
        return next.run(req).await;
    }
    let origin_ok = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|o| state.is_dashboard_origin(o));
    if !origin_ok {
        tracing::warn!(path = %req.uri().path(), "rejected cross-origin or origin-less mutation");
        return AppError::Forbidden.into_response();
    }
    next.run(req).await
}

pub async fn security_headers(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let set = |h: &mut axum::http::HeaderMap, k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(
        h,
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: https://avatars.githubusercontent.com; font-src 'self'; \
         connect-src 'self'; object-src 'none'; base-uri 'none'; \
         form-action 'self' https://github.com; \
         frame-ancestors 'none'",
    );
    set(h, "x-content-type-options", "nosniff");
    set(h, "x-frame-options", "DENY");
    set(h, "referrer-policy", "strict-origin-when-cross-origin");
    set(h, "cross-origin-opener-policy", "same-origin");
    set(h, "cross-origin-resource-policy", "same-origin");
    set(
        h,
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), payment=()",
    );
    if state.cfg.secure_cookies() {
        set(
            h,
            "strict-transport-security",
            "max-age=63072000; includeSubDomains",
        );
    }
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    res
}
