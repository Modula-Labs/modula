mod common;

use axum::http::{Method, Request, StatusCode, header};
use common::*;
use sqlx::PgPool;

fn web_built() -> bool {
    std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/dist/index.html"
    ))
    .exists()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn serves_spa_routes_and_assets(pool: PgPool) {
    let app = TestApp::new(pool);
    if !web_built() {
        let r = app.call(Method::GET, "/projects", None, None).await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
        eprintln!("skipping asset checks: web/dist not built");
        return;
    }
    for path in ["/", "/projects", "/login/2fa"] {
        let r = app.call(Method::GET, path, None, None).await;
        assert_eq!(r.status, StatusCode::OK, "{path}");
        assert!(
            r.headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert_eq!(r.headers[header::CACHE_CONTROL], "no-cache");
        assert!(
            r.headers["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("script-src 'self'")
        );
    }

    // Find the hashed JS bundle referenced by index.html and fetch it.
    let index = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/dist/index.html"
    ))
    .unwrap();
    let js = index
        .split("src=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let r = app.call(Method::GET, &js, None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    let etag = r.headers[header::ETAG].to_str().unwrap().to_owned();
    let r = app
        .send(
            Request::builder()
                .uri(&js)
                .header(header::IF_NONE_MATCH, &etag),
            None,
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn spa_fallback_does_not_leak_or_shadow(pool: PgPool) {
    let app = TestApp::new(pool);
    // Unknown API paths stay JSON 404s.
    let r = app
        .call(Method::GET, "/api/does-not-exist", None, None)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.code(), "not_found");
    // Missing files and traversal attempts are plain 404s.
    for path in [
        "/missing.js",
        "/assets/nope.css",
        "/../Cargo.toml",
        "/assets/..%2F..%2FCargo.toml",
        "/%2e%2e/secret",
    ] {
        let r = app.call(Method::GET, path, None, None).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path}");
    }
    // Non-GET on a UI path is not served (and is still Origin-checked first).
    let r = app.call(Method::POST, "/projects", None, None).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
}
