//! Following install.sh on the new server without SSH. Given
//! `--progress-token`, the installer serves its step list read-only on port
//! 80 at `/.modula-progress/<token>.json` until Caddy takes the port over
//! (`state: "handoff"`), or for ten minutes after a failure (`state:
//! "failed"` with the installer's message and hint). The token is random
//! (256 bits) and only names the file; nothing secret is in it.

use std::net::Ipv4Addr;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Report {
    /// `running`, `failed` or `handoff`.
    pub state: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub error: Option<InstallError>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Step {
    pub name: String,
    /// `running`, `done` or `skipped`.
    pub state: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct InstallError {
    #[serde(default)]
    pub step: String,
    pub message: String,
    #[serde(default)]
    pub hint: String,
}

/// 32 random bytes, base64url (what install.sh's `--progress-token` takes).
pub fn new_token() -> anyhow::Result<String> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(raw))
}

pub struct Reporter {
    http: reqwest::Client,
    url: Url,
}

impl Reporter {
    /// `http://<ip>/.modula-progress/<token>.json`, or under `via` (tests).
    pub fn new(ip: Ipv4Addr, token: &str, via: Option<&Url>) -> anyhow::Result<Self> {
        let base = match via {
            Some(u) => u.clone(),
            None => Url::parse(&format!("http://{ip}/"))?,
        };
        let url = base.join(&format!(".modula-progress/{token}.json"))?;
        crate::api::install_crypto();
        let http = reqwest::Client::builder()
            .user_agent(crate::api::USER_AGENT)
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { http, url })
    }

    /// The installer's report; `Err` while there is none (booting, or after
    /// the hand-off).
    pub async fn fetch(&self) -> Result<Report, String> {
        let res = self
            .http
            .get(self.url.clone())
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !res.status().is_success() {
            return Err(format!("HTTP {}", res.status().as_u16()));
        }
        res.json().await.map_err(|e| e.without_url().to_string())
    }
}

/// The installer's step names, said the way the CLI says things.
pub fn friendly(name: &str) -> String {
    match name {
        "Checking this server" => "Checking the server".into(),
        "Installing packages" => "Updating packages, installing Docker, Postgres & Redis".into(),
        "Setting up the firewall" => "Setting up the firewall & hardening".into(),
        "Creating users and directories" => "Creating users and directories".into(),
        n if n.starts_with("Downloading release") => {
            n.replacen("Downloading release", "Downloading Modula", 1)
        }
        n => n.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_install_sh_writes() {
        // Verbatim from install.sh (see its progress_write).
        let r: Report = serde_json::from_str(
            r#"{"state":"failed","steps":[{"name":"Firewall","state":"skipped","at":1792784619},{"name":"Installing packages","state":"done","at":1792784619},{"name":"Setting up Postgres","state":"running","at":1792784619}],"error":{"step":"Setting up Postgres","message":"Postgres didn't start","hint":"Check \"journalctl -u postgresql\"."}}"#,
        )
        .unwrap();
        assert_eq!(r.state, "failed");
        assert_eq!(r.steps.len(), 3);
        assert_eq!(r.error.unwrap().hint, "Check \"journalctl -u postgresql\".");
        let r: Report = serde_json::from_str(r#"{"state":"handoff","steps":[]}"#).unwrap();
        assert!(r.error.is_none());
    }

    #[test]
    fn tokens_fit_install_sh() {
        let t = new_token().unwrap();
        assert_eq!(t.len(), 43);
        assert!(
            t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn friendly_names() {
        assert_eq!(
            friendly("Downloading release (latest)"),
            "Downloading Modula (latest)"
        );
        assert_eq!(friendly("Setting up Redis"), "Setting up Redis");
    }

    #[test]
    fn url_shape() {
        let r = Reporter::new("203.0.113.10".parse().unwrap(), "tok", None).unwrap();
        assert_eq!(
            r.url.as_str(),
            "http://203.0.113.10/.modula-progress/tok.json"
        );
    }
}
