//! GitHub REST client: resolving commits and building source-archive URLs.
//!
//! Repositories are fetched by the node as tarballs at an exact commit, never
//! cloned: no git binary, no credentials in a URL or argv, no `.git` directory.
//! Private repositories need an installation token (GitHub App, see `app`).

pub mod app;
pub mod push;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::error::{AppError, AppResult};

const USER_AGENT: &str = concat!("modula-deploy/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    api: Url,
    /// Installation tokens, reused until shortly before they expire.
    tokens: Arc<Mutex<HashMap<app::TokenKey, app::CachedToken>>>,
    /// Repository and branch listings for the pickers, kept briefly so typing
    /// in a combobox doesn't spend GitHub's rate limit.
    lists: Arc<Mutex<app::ListCache>>,
}

/// The head of a branch, as recorded on a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub message: String,
    pub author: String,
    /// The author's GitHub login (for the avatar), when GitHub linked one.
    pub author_login: Option<String>,
}

/// GitHub logins are 1–39 alphanumerics and hyphens.
pub fn valid_login(s: &str) -> bool {
    (1..=39).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Client {
    /// `api` is `https://api.github.com` outside tests.
    pub fn new(api: Url) -> anyhow::Result<Self> {
        // reqwest uses the process-wide rustls provider; ring, like the rest
        // of the server. Installing twice is a no-op.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            api,
            tokens: Arc::default(),
            lists: Arc::default(),
        })
    }

    /// `path` may carry a query string.
    fn url(&self, path: &str) -> Url {
        let (path, query) = path
            .split_once('?')
            .map_or((path, None), |(p, q)| (p, Some(q)));
        let mut u = self.api.clone();
        u.set_path(&format!("{}{path}", self.api.path().trim_end_matches('/')));
        u.set_query(query);
        u
    }

    /// A REST request with GitHub's versioned media type. `path` must only
    /// contain validated segments (repo names, hex ids, integers).
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// The commit a branch points at. `token` is an installation token for
    /// private repositories, `None` for public ones.
    pub async fn branch_head(
        &self,
        repo: &str,
        branch: &str,
        token: Option<&str>,
    ) -> AppResult<Commit> {
        #[derive(Deserialize)]
        struct Resp {
            sha: String,
            commit: Inner,
            author: Option<User>,
        }
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        #[derive(Deserialize)]
        struct Inner {
            message: String,
            author: Option<Author>,
        }
        #[derive(Deserialize)]
        struct Author {
            name: String,
        }
        // The branch goes in the path, so encode every segment ("release/1.2").
        let mut url = self.url(&format!("/repos/{repo}/commits"));
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("bad GitHub API URL"))?
            .push(branch);
        let mut req = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| {
            tracing::warn!(error = %e, "GitHub API unreachable");
            AppError::Unavailable("GitHub could not be reached; try again".into())
        })?;
        match resp.status().as_u16() {
            200 => {}
            404 | 422 => {
                return Err(AppError::bad_request(format!(
                    "{repo} has no branch {branch}, or the repository is private"
                )));
            }
            403 | 429 => {
                return Err(AppError::Unavailable(
                    "GitHub's API rate limit was reached; try again in a few minutes".into(),
                ));
            }
            s => {
                tracing::warn!(status = s, "unexpected GitHub API response");
                return Err(AppError::Unavailable(
                    "GitHub returned an error; try again".into(),
                ));
            }
        }
        let r: Resp = resp.json().await.map_err(anyhow::Error::from)?;
        if r.sha.len() != 40
            || !r
                .sha
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(anyhow::anyhow!("GitHub returned a malformed commit id").into());
        }
        Ok(Commit {
            sha: r.sha,
            message: r.commit.message.chars().take(1000).collect(),
            author: r
                .commit
                .author
                .map(|a| a.name)
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
            author_login: r.author.map(|u| u.login).filter(|l| valid_login(l)),
        })
    }

    /// Where the node downloads the source at one commit (gzipped tarball).
    pub fn tarball_url(&self, repo: &str, sha: &str) -> String {
        self.url(&format!("/repos/{repo}/tarball/{sha}"))
            .to_string()
    }
}
