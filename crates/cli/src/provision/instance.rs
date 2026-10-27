//! Talking to the new instance before anyone is logged in: is it up, then
//! finish setup with the preseeded token and mint an API token for this
//! machine.
//!
//! Requests go to `https://<domain>` but connect to the server's IP directly
//! (so a stale local DNS cache can't stall us once public DNS is right).
//! Mutations carry `Origin: https://<domain>`, as the dashboard's own would:
//! the server only accepts cookie-authenticated writes from its own origin.
//! Creating an API token needs a fresh ("sudo") session, which the session
//! that setup returns is.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::header;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;
use url::Url;

pub struct Instance {
    http: reqwest::Client,
    /// Where requests go (normally `https://<domain>/`).
    base: Url,
    /// `https://<domain>`.
    origin: String,
}

impl Instance {
    /// `via` replaces where requests go (tests); else `https://<domain>`
    /// pinned to `ip`.
    pub fn new(domain: &str, ip: IpAddr, via: Option<&Url>) -> anyhow::Result<Self> {
        crate::api::install_crypto();
        let mut b = reqwest::Client::builder()
            .user_agent(crate::api::USER_AGENT)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none());
        if via.is_none() {
            b = b.resolve(domain, SocketAddr::new(ip, 443));
        }
        let origin = format!("https://{domain}");
        let base = match via {
            Some(u) => u.clone(),
            None => Url::parse(&format!("{origin}/"))?,
        };
        Ok(Self {
            http: b.build()?,
            base,
            origin,
        })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    fn url(&self, path: &str) -> anyhow::Result<Url> {
        Ok(self.base.join(path.trim_start_matches('/'))?)
    }

    /// Up and serving its API over TLS. Any failure (connection refused, no
    /// certificate yet, 502 from the proxy) just means "not yet": the reason
    /// comes back for the progress line.
    pub async fn health(&self) -> Result<(), String> {
        let res = self
            .http
            .get(self.url("/api/health").map_err(|e| e.to_string())?)
            .send()
            .await
            .map_err(|e| short_error(&e))?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!("HTTP {}", res.status().as_u16()))
        }
    }

    /// Whether setup is still pending.
    pub async fn setup_required(&self) -> anyhow::Result<bool> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Status {
            setup_required: bool,
        }
        let s: Status = self
            .http
            .get(self.url("/api/instance")?)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(s.setup_required)
    }

    /// Create the admin; returns the session cookie (`name=value`).
    pub async fn setup(
        &self,
        setup_token: &SecretString,
        email: &str,
        name: &str,
        password: &SecretString,
    ) -> anyhow::Result<SecretString> {
        let body = json!({
            "setupToken": setup_token.expose_secret(),
            "email": email,
            "name": name,
            "password": password.expose_secret(),
        });
        let res = self
            .http
            .post(self.url("/api/auth/setup")?)
            .header(header::ORIGIN, &self.origin)
            .json(&body)
            .send()
            .await
            .context("cannot reach the new server")?;
        let status = res.status();
        let cookie = res
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .find(|kv| {
                kv.split_once('=')
                    .is_some_and(|(k, v)| k.ends_with("modula_session") && !v.is_empty())
            })
            .map(|kv| SecretString::from(kv.to_owned()));
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            match status.as_u16() {
                401 => bail!(
                    "the server rejected the setup token (was it restarted by hand with another one?)"
                ),
                404 => bail!(
                    "setup was already completed on this server; log in with `modula login {}`",
                    self.origin
                ),
                _ => bail!("setup failed (HTTP {status}): {}", api_message(&text)),
            }
        }
        cookie.context("setup succeeded but returned no session")
    }

    /// An API token for this machine, created with the setup session.
    pub async fn create_token(
        &self,
        session_cookie: &SecretString,
        name: &str,
    ) -> anyhow::Result<SecretString> {
        #[derive(Deserialize)]
        struct Created {
            token: String,
        }
        let res = self
            .http
            .post(self.url("/api/account/tokens")?)
            .header(header::ORIGIN, &self.origin)
            .header(header::COOKIE, session_cookie.expose_secret())
            .json(&json!({ "name": name }))
            .send()
            .await
            .context("cannot reach the new server")?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "creating an API token failed (HTTP {status}): {}",
                api_message(&text)
            );
        }
        let created: Created =
            serde_json::from_str(&text).context("unexpected response creating the API token")?;
        Ok(SecretString::from(created.token))
    }
}

impl Instance {
    /// Save the tokens `server create` used as the first workspace's Hetzner
    /// and Cloudflare integrations, so the dashboard can add servers and
    /// manage DNS without asking again. Each result is independent.
    pub async fn save_integrations(
        &self,
        session_cookie: &SecretString,
        hetzner: &SecretString,
        cloudflare: Option<(&SecretString, Option<&str>)>,
    ) -> anyhow::Result<Vec<(&'static str, anyhow::Result<()>)>> {
        #[derive(Deserialize)]
        struct Workspace {
            id: String,
        }
        let res = self
            .http
            .get(self.url("/api/workspaces")?)
            .header(header::ORIGIN, &self.origin)
            .header(header::COOKIE, session_cookie.expose_secret())
            .send()
            .await
            .context("cannot reach the new server")?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "listing workspaces failed (HTTP {status}): {}",
                api_message(&text)
            );
        }
        let ws: Vec<Workspace> =
            serde_json::from_str(&text).context("unexpected workspaces response")?;
        let ws = ws.first().context("the new admin has no workspace")?;
        let mut out = vec![(
            "Hetzner",
            self.put_integration(
                session_cookie,
                &ws.id,
                "hetzner",
                json!({ "token": hetzner.expose_secret() }),
            )
            .await,
        )];
        if let Some((token, account)) = cloudflare {
            let mut body = json!({ "token": token.expose_secret() });
            if let Some(a) = account {
                body["accountId"] = json!(a);
            }
            out.push((
                "Cloudflare",
                self.put_integration(session_cookie, &ws.id, "cloudflare", body)
                    .await,
            ));
        }
        Ok(out)
    }

    async fn put_integration(
        &self,
        session_cookie: &SecretString,
        workspace: &str,
        provider: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<()> {
        let res = self
            .http
            .put(self.url(&format!(
                "/api/workspaces/{workspace}/integrations/{provider}"
            ))?)
            .header(header::ORIGIN, &self.origin)
            .header(header::COOKIE, session_cookie.expose_secret())
            .json(&body)
            .send()
            .await
            .context("cannot reach the new server")?;
        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            bail!("HTTP {status}: {}", api_message(&text));
        }
        Ok(())
    }
}

/// `{"error":{"message":…}}` → the message; else the text, cut short.
fn api_message(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| text.chars().take(200).collect())
}

/// The innermost cause of a request error, without the URL.
fn short_error(e: &reqwest::Error) -> String {
    let mut source: &dyn std::error::Error = e;
    while let Some(inner) = source.source() {
        source = inner;
    }
    let text = source.to_string();
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() && text.to_ascii_lowercase().contains("refused") {
        "connection refused".into()
    } else {
        text
    }
}
