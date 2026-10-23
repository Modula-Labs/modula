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
            (401, _) => write!(
                f,
                "the API token was rejected (expired or revoked?); run `modula-deploy login`"
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
    http: reqwest::Client,
    /// Without an overall timeout, for log streams that stay open.
    streaming: reqwest::Client,
}

impl Api {
    pub fn new(login: Login) -> anyhow::Result<Self> {
        install_crypto();
        let builder = || {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
        };
        let http = builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("cannot set up the HTTP client")?;
        let streaming = builder().build().context("cannot set up the HTTP client")?;
        Ok(Self {
            login,
            http,
            streaming,
        })
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
            .streaming
            .get(self.url(path)?)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .bearer_auth(&self.login.token)
            .send()
            .await
            .with_context(|| format!("cannot reach {}", self.login.url))?;
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
        let res = req
            .bearer_auth(&self.login.token)
            .send()
            .await
            .with_context(|| format!("cannot reach {}", self.login.url))?;
        let status = res.status();
        let retry_after = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        let body = res.bytes().await.context("reading the response failed")?;
        if !status.is_success() {
            let mut e = ApiError::from_body(status.as_u16(), &body);
            e.retry_after = retry_after;
            return Err(e.into());
        }
        serde_json::from_slice(&body).with_context(|| format!("unexpected response from {path}"))
    }
}
