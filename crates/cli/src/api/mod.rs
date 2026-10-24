//! A small client for the Modula Deploy HTTP API, authenticated with an API
//! token (`Authorization: Bearer mdt_…`). [`endpoints`] has one typed method
//! per route the CLI uses; [`types`] the request and response shapes.

pub mod endpoints;
pub mod types;

use std::time::Duration;

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::Login;

pub use types::*;

pub const USER_AGENT: &str = concat!("modula-deploy/", env!("CARGO_PKG_VERSION"));

/// An error response from the API (`{"error": {"code", "message"}}`).
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    /// From `Retry-After` on 429s.
    pub retry_after: Option<u64>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.status, self.code.as_str()) {
            (_, "access_required") => write!(f, "{}", self.message),
            (401, _) => write!(
                f,
                "the API token was rejected (expired or revoked?); run `modula login`"
            ),
            (_, "sudo_required") => write!(
                f,
                "this needs a recently re-authenticated dashboard session; API tokens never \
                 have that, so do it in the dashboard"
            ),
            (429, _) => match self.retry_after {
                Some(s) => write!(f, "{} (try again in {s}s)", self.message),
                None => write!(f, "{} (HTTP 429)", self.message),
            },
            _ => write!(f, "{} (HTTP {})", self.message, self.status),
        }
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    pub fn from_body(status: u16, body: &[u8]) -> Self {
        #[derive(Deserialize)]
        struct Body {
            error: Inner,
        }
        #[derive(Deserialize)]
        struct Inner {
            code: String,
            message: String,
        }
        match serde_json::from_slice::<Body>(body) {
            Ok(b) => Self {
                status,
                code: b.error.code,
                message: b.error.message,
                retry_after: None,
            },
            Err(_) => Self {
                status,
                code: String::new(),
                message: format!("unexpected response (HTTP {status})"),
                retry_after: None,
            },
        }
    }
}

/// The API error behind `e`, if that's what it is.
pub fn api_error(e: &anyhow::Error) -> Option<&ApiError> {
    e.downcast_ref::<ApiError>()
}

/// Install ring as the process-wide rustls provider (no aws-lc, no OpenSSL).
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub struct Api {
    pub login: Login,
    /// Cloudflare Access credentials in use (an app token is renewed here).
    access: std::sync::Mutex<Option<crate::access::CfAccess>>,
    http: reqwest::Client,
    /// Without an overall timeout, for log streams that stay open.
    streaming: reqwest::Client,
}

impl Api {
    pub fn new(login: Login) -> anyhow::Result<Self> {
        install_crypto();
        // Redirects are never followed: the API doesn't redirect, and
        // Cloudflare Access's sign-in redirect must be seen, not followed.
        let builder = || {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
        };
        let http = builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("cannot set up the HTTP client")?;
        let streaming = builder().build().context("cannot set up the HTTP client")?;
        Ok(Self {
            access: std::sync::Mutex::new(login.access.clone()),
            login,
            http,
            streaming,
        })
    }

    /// The Cloudflare Access credentials currently in use.
    pub fn access(&self) -> Option<crate::access::CfAccess> {
        self.access.lock().expect("access lock").clone()
    }

    /// API token and Access headers.
    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut req = req.bearer_auth(&self.login.token);
        if let Some(a) = self.access() {
            for (k, v) in a.headers() {
                req = req.header(k, v);
            }
        }
        req
    }

    /// Send, renewing an expired Access app token once (when the request can
    /// be repeated). An Access challenge becomes an `access_required` error.
    async fn execute(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let retry = req.try_clone();
        let res = self
            .authed(req)
            .send()
            .await
            .with_context(|| format!("cannot reach {}", self.login.url))?;
        if !self.is_challenge(&res) {
            return Ok(res);
        }
        let renewable = matches!(
            self.access(),
            Some(crate::access::CfAccess::AppToken { .. })
        );
        if let (true, Some(retry)) = (renewable, retry)
            && let Ok(fresh) = crate::access::app_token(&self.login.url)
        {
            *self.access.lock().expect("access lock") = Some(fresh);
            let res = self
                .authed(retry)
                .send()
                .await
                .with_context(|| format!("cannot reach {}", self.login.url))?;
            if !self.is_challenge(&res) {
                return Ok(res);
            }
        }
        Err(self.access_error(res.status().as_u16()).into())
    }

    fn is_challenge(&self, res: &reqwest::Response) -> bool {
        let status = res.status().as_u16();
        let location = res
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok());
        // A 403 is only Access's when the body says so; checked in `send`.
        (300..400).contains(&status) && crate::access::is_access_challenge(status, location, b"")
    }

    fn access_error(&self, status: u16) -> ApiError {
        ApiError {
            status,
            code: "access_required".into(),
            message: crate::access::help(&self.login.url),
            retry_after: None,
        }
    }

    pub fn url(&self, path: &str) -> anyhow::Result<Url> {
        self.login
            .url
            .join(path.trim_start_matches('/'))
            .with_context(|| format!("invalid API path {path}"))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        self.send(self.http.get(self.url(path)?), path).await
    }

    /// POST without a body (e.g. revealing a variable).
    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        self.send(self.http.post(self.url(path)?), path).await
    }

    pub async fn post<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        self.send(self.http.post(self.url(path)?).json(body), path)
            .await
    }

    pub async fn put<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        self.send(self.http.put(self.url(path)?).json(body), path)
            .await
    }

    pub async fn patch<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        self.send(self.http.patch(self.url(path)?).json(body), path)
            .await
    }

    /// POST a streamed body (source uploads), without an overall timeout.
    pub async fn post_body<T: DeserializeOwned>(
        &self,
        path: &str,
        headers: reqwest::header::HeaderMap,
        body: reqwest::Body,
    ) -> anyhow::Result<T> {
        let req = self.streaming.post(self.url(path)?).headers(headers);
        self.send(req.body(body), path).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        self.send(self.http.delete(self.url(path)?), path).await
    }

    /// A GET whose body is read incrementally (Server-Sent Events).
    pub async fn stream(&self, path: &str) -> anyhow::Result<reqwest::Response> {
        let res = self
            .execute(
                self.streaming
                    .get(self.url(path)?)
                    .header(reqwest::header::ACCEPT, "text/event-stream"),
            )
            .await?;
        if res.status().is_success() {
            return Ok(res);
        }
        let status = res.status().as_u16();
        let body = res.bytes().await.unwrap_or_default();
        Err(ApiError::from_body(status, &body).into())
    }

    async fn send<T: DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        path: &str,
    ) -> anyhow::Result<T> {
        let res = self.execute(req).await?;
        let status = res.status();
        let retry_after = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        let body = res.bytes().await.context("reading the response failed")?;
        if crate::access::is_access_challenge(status.as_u16(), None, &body) {
            return Err(self.access_error(status.as_u16()).into());
        }
        if !status.is_success() {
            let mut e = ApiError::from_body(status.as_u16(), &body);
            e.retry_after = retry_after;
            return Err(e.into());
        }
        serde_json::from_slice(&body).with_context(|| format!("unexpected response from {path}"))
    }
}
