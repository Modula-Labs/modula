#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use modula_crypto::envelope::MasterKey;
use modula_server::config::Config;
use modula_server::keyring::Keyring;
use modula_server::ratelimit::RateLimiter;
use modula_server::state::AppState;
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub const ORIGIN: &str = "https://cloud.test";
pub const COOKIE: &str = "__Host-modula_session";
pub const PASSWORD: &str = "correct horse battery staple";

pub fn config() -> Config {
    Config {
        database_url: String::new(),
        redis_url: String::new(),
        public_url: ORIGIN.parse().unwrap(),
        listen: "127.0.0.1:0".parse().unwrap(),
        master_key_file: PathBuf::new(),
        trusted_proxies: vec!["127.0.0.1".parse().unwrap()],
    }
}

pub fn state(pool: PgPool) -> AppState {
    AppState::new(
        pool,
        config(),
        Keyring::new(MasterKey::generate().unwrap()),
        RateLimiter::memory(),
    )
}

pub struct TestApp {
    pub state: AppState,
    pub router: Router,
}

impl TestApp {
    pub fn new(pool: PgPool) -> Self {
        let state = state(pool);
        let router = modula_server::http::app(state.clone())
            .layer(MockConnectInfo(SocketAddr::from(([10, 0, 0, 1], 4000))));
        Self { state, router }
    }

    pub fn db(&self) -> &PgPool {
        &self.state.db
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> Resp {
        let mut b = Request::builder().method(method.clone()).uri(path);
        if method != Method::GET {
            b = b.header(header::ORIGIN, ORIGIN);
        }
        self.send(b, cookie, body).await
    }

    pub async fn send(
        &self,
        mut b: axum::http::request::Builder,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> Resp {
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, format!("{COOKIE}={c}"));
        }
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Resp {
            status,
            headers,
            json,
        }
    }

    /// Complete first-run setup and return the admin's session cookie value.
    pub async fn setup_admin(&self) -> String {
        let token = modula_server::auth::setup::issue_if_pending(self.db())
            .await
            .unwrap()
            .unwrap();
        let r = self
            .call(
                Method::POST,
                "/api/auth/setup",
                None,
                Some(json!({
                    "setupToken": token.expose_secret(),
                    "email": "admin@example.com",
                    "name": "Admin",
                    "password": PASSWORD,
                })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.session_cookie().expect("setup sets a session cookie")
    }

    /// Insert a user directly and log them in.
    pub async fn user(&self, email: &str) -> (Uuid, String) {
        let id = Uuid::now_v7();
        let hash = modula_crypto::password::hash(PASSWORD).unwrap();
        sqlx::query("INSERT INTO users (id, email, name, password_hash) VALUES ($1, $2, 'U', $3)")
            .bind(id)
            .bind(email)
            .bind(hash)
            .execute(self.db())
            .await
            .unwrap();
        (id, self.login(email).await)
    }

    pub async fn login(&self, email: &str) -> String {
        let r = self
            .call(
                Method::POST,
                "/api/auth/login",
                None,
                Some(json!({"email": email, "password": PASSWORD})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.session_cookie().unwrap()
    }

    pub async fn create_workspace(&self, cookie: &str, name: &str) -> Uuid {
        let r = self
            .call(
                Method::POST,
                "/api/workspaces",
                Some(cookie),
                Some(json!({"name": name})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.json["id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn add_member(&self, workspace: Uuid, user: Uuid, role: &str) {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)",
        )
        .bind(workspace)
        .bind(user)
        .bind(role)
        .execute(self.db())
        .await
        .unwrap();
    }
}

#[derive(Debug)]
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub json: Value,
}

impl Resp {
    pub fn session_cookie(&self) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(&format!("{COOKIE}=")))
            .and_then(|v| v.split(';').next())
            .and_then(|kv| kv.split_once('='))
            .map(|(_, v)| v.to_owned())
            .filter(|v| !v.is_empty())
    }

    pub fn set_cookie_header(&self) -> Option<String> {
        self.headers
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    pub fn code(&self) -> &str {
        self.json["error"]["code"].as_str().unwrap_or("")
    }
}
