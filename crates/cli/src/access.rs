//! Instances in private mode sit behind Cloudflare Access: every request must
//! also carry Access credentials, next to the API token. Two kinds:
//!
//! - a service token (CI, scripts): `MODULA_CF_ACCESS_CLIENT_ID` and
//!   `MODULA_CF_ACCESS_CLIENT_SECRET`, sent as `CF-Access-Client-Id` /
//!   `CF-Access-Client-Secret`;
//! - a person's app token from `cloudflared access login` (browser sign-in),
//!   sent as `cf-access-token` and renewed with `cloudflared access token`
//!   when Access starts redirecting again.
//!
//! Both are saved next to the API token (same file, mode 0600).

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use url::Url;

pub const ENV_CLIENT_ID: &str = "MODULA_CF_ACCESS_CLIENT_ID";
pub const ENV_CLIENT_SECRET: &str = "MODULA_CF_ACCESS_CLIENT_SECRET";

/// Credentials for Cloudflare Access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CfAccess {
    ServiceToken {
        client_id: String,
        client_secret: String,
    },
    /// An application token from `cloudflared access token`.
    AppToken { token: String },
}

impl CfAccess {
    /// The headers that go with every request (HTTP and WebSocket).
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::ServiceToken {
                client_id,
                client_secret,
            } => vec![
                ("cf-access-client-id", client_id.clone()),
                ("cf-access-client-secret", client_secret.clone()),
            ],
            Self::AppToken { token } => vec![("cf-access-token", token.clone())],
        }
    }

    /// A service token from the environment, if both variables are set.
    pub fn from_env(id: Option<String>, secret: Option<String>) -> Option<Self> {
        match (id, secret) {
            (Some(client_id), Some(client_secret))
                if !client_id.is_empty() && !client_secret.is_empty() =>
            {
                Some(Self::ServiceToken {
                    client_id,
                    client_secret,
                })
            }
            _ => None,
        }
    }

    pub fn from_process_env() -> Option<Self> {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_owned());
        Self::from_env(var(ENV_CLIENT_ID), var(ENV_CLIENT_SECRET))
    }
}

/// Whether a response is Cloudflare Access asking to sign in (instead of the
/// API answering), or the instance refusing a request that skipped Access.
pub fn is_access_challenge(status: u16, location: Option<&str>, body: &[u8]) -> bool {
    let redirect = (300..400).contains(&status)
        && location.and_then(|l| Url::parse(l).ok()).is_some_and(|u| {
            u.host_str()
                .is_some_and(|h| h.ends_with(".cloudflareaccess.com"))
                || u.path().starts_with("/cdn-cgi/access/")
        });
    let refused = status == 403
        && serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .is_some_and(|v| v["error"]["code"] == "access_required");
    redirect || refused
}

/// What to tell someone who can't get through Access.
pub fn help(url: &Url) -> String {
    format!(
        "{url} is private (Cloudflare Access). Either install cloudflared and run `modula login` again \
         (it opens your browser to sign in), or create a service token in Cloudflare Zero Trust \
         (Access → Service credentials) and set {ENV_CLIENT_ID} and {ENV_CLIENT_SECRET}."
    )
}

fn cloudflared() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| {
            d.join(if cfg!(windows) {
                "cloudflared.exe"
            } else {
                "cloudflared"
            })
        })
        .find(|p| p.is_file())
}

pub fn has_cloudflared() -> bool {
    cloudflared().is_some()
}

/// Signs in through the browser (`cloudflared access login`: a local callback,
/// like `gh auth login`) and returns the app token.
pub fn browser_login(url: &Url) -> anyhow::Result<CfAccess> {
    let bin = cloudflared().context("cloudflared is not installed")?;
    let status = std::process::Command::new(&bin)
        .args(["access", "login", url.as_str()])
        .status()
        .context("cannot run cloudflared")?;
    if !status.success() {
        bail!("cloudflared access login failed");
    }
    app_token(url)
}

/// The current app token from cloudflared's cache (renewed by cloudflared if
/// its session still allows; no browser).
pub fn app_token(url: &Url) -> anyhow::Result<CfAccess> {
    let bin = cloudflared().context("cloudflared is not installed")?;
    let out = std::process::Command::new(&bin)
        .args(["access", "token", &format!("-app={}", url.as_str())])
        .output()
        .context("cannot run cloudflared")?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || token.is_empty() || token.contains(char::is_whitespace) {
        bail!("no Cloudflare Access session for {url}; run `modula login` again");
    }
    Ok(CfAccess::AppToken { token })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_per_kind() {
        let s = CfAccess::ServiceToken {
            client_id: "id.access".into(),
            client_secret: "sec".into(),
        };
        assert_eq!(
            s.headers(),
            [
                ("cf-access-client-id", "id.access".to_owned()),
                ("cf-access-client-secret", "sec".to_owned())
            ]
        );
        let t = CfAccess::AppToken {
            token: "jwt".into(),
        };
        assert_eq!(t.headers(), [("cf-access-token", "jwt".to_owned())]);
    }

    #[test]
    fn env_needs_both_halves() {
        assert!(CfAccess::from_env(Some("a".into()), None).is_none());
        assert!(CfAccess::from_env(Some("a".into()), Some(String::new())).is_none());
        assert!(CfAccess::from_env(Some("a".into()), Some("b".into())).is_some());
    }

    #[test]
    fn challenges() {
        assert!(is_access_challenge(
            302,
            Some("https://acme.cloudflareaccess.com/cdn-cgi/access/login/x"),
            b""
        ));
        assert!(is_access_challenge(
            302,
            Some("https://deploy.example.com/cdn-cgi/access/login"),
            b""
        ));
        assert!(!is_access_challenge(
            302,
            Some("https://deploy.example.com/login"),
            b""
        ));
        assert!(is_access_challenge(
            403,
            None,
            br#"{"error":{"code":"access_required","message":"x"}}"#
        ));
        assert!(!is_access_challenge(
            403,
            None,
            br#"{"error":{"code":"forbidden","message":"x"}}"#
        ));
    }

    #[test]
    fn saved_form_is_tagged() {
        let t = CfAccess::AppToken { token: "j".into() };
        let s = toml::to_string(&t).unwrap();
        assert!(s.contains("kind = \"app_token\""));
        assert_eq!(toml::from_str::<CfAccess>(&s).unwrap(), t);
    }
}
