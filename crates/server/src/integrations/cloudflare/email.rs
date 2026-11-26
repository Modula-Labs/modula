//! Sending email through Cloudflare Email Service (Email Sending, beta).
//!
//! `POST /accounts/{account_id}/email/sending/send` with a JSON body
//! (`from`, `to`, `subject`, `text`, `html`), authorized by the workspace's
//! API token, which needs the permission "Email Sending: Edit" (API name
//! `Email Sending Write`; Cloudflare doesn't document whether it is account-
//! or zone-scoped). The sender's domain must be onboarded
//! for Email Sending on that account (dashboard: Compute → Email Service →
//! Email Sending → Onboard Domain). On the Workers Free plan only verified
//! destination addresses receive mail; Workers Paid sends to anyone.
//!
//! [`EmailProvider`] is the seam for another provider later (SMTP, SES):
//! alerts only see [`Email`] and [`SendError`].

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::client::{self, Client};

/// One message: plain text plus a simple HTML alternative.
#[derive(Debug, Clone, PartialEq)]
pub struct Email {
    pub from: String,
    pub from_name: String,
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: String,
}

/// What went wrong, worded so the dashboard can tell the user what to fix.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct SendError {
    /// The provider's HTTP status, when it answered.
    pub status: Option<u16>,
    pub message: String,
}

/// Sends email. One implementation per provider.
pub trait EmailProvider {
    fn send(
        &self,
        email: &Email,
    ) -> impl std::future::Future<Output = Result<Sent, SendError>> + Send;
}

/// The provider accepted the message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sent {
    pub message_id: Option<String>,
}

/// Cloudflare Email Sending for one account.
pub struct Cloudflare<'a> {
    pub client: &'a Client,
    pub account_id: &'a str,
}

#[derive(Deserialize, Default)]
struct SendResult {
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    permanent_bounces: Vec<serde_json::Value>,
    #[serde(default)]
    suppressed_recipients: Vec<serde_json::Value>,
}

/// The request body Cloudflare's REST API takes.
pub fn body(e: &Email) -> serde_json::Value {
    json!({
        "from": { "address": e.from, "name": e.from_name },
        "to": [e.to],
        "subject": e.subject,
        "text": e.text,
        "html": e.html,
    })
}

/// Maps a Cloudflare error to what the user must do. The codes are the
/// Email Sending API's (`email.sending.error.*`).
pub fn explain(e: &client::Error) -> SendError {
    let status = e.status();
    let raw = e.to_string();
    let has = |code: &str| raw.contains(&format!("[{code}]"));
    let message = if has("10208") {
        "Cloudflare only delivers to verified destination addresses on this plan: verify the \
         recipient under Email Routing → Destination addresses, or upgrade to Workers Paid"
            .to_owned()
    } else if has("10105") {
        "this Cloudflare account isn't entitled to Email Sending: enable Email Service on the account"
            .to_owned()
    } else if has("10203") {
        "Email Sending is disabled on this Cloudflare account".to_owned()
    } else if has("10207") {
        "the Cloudflare account is suspended for sending".to_owned()
    } else if matches!(status, Some(401 | 403)) {
        "the Cloudflare token can't send email: add the permission Email Sending · Edit \
         to the token, and onboard the sender's domain under Email Service → Email Sending"
            .to_owned()
    } else if status == Some(429) {
        "Cloudflare's daily sending quota is used up; try again later".to_owned()
    } else if has("10202") || has("10001") {
        format!("Cloudflare refused the message (check the sender and recipient addresses): {raw}")
    } else {
        return SendError { status, message: raw };
    };
    // Keep Cloudflare's own words next to the explanation: the codes it
    // returns aren't all documented, and the reply is what tells them apart.
    let message = match e {
        client::Error::Api { .. } if !message.contains(&raw) => format!("{message} ({raw})"),
        _ => message,
    };
    SendError { status, message }
}

impl EmailProvider for Cloudflare<'_> {
    async fn send(&self, email: &Email) -> Result<Sent, SendError> {
        let path = format!("accounts/{}/email/sending/send", self.account_id);
        let res: Result<(Option<SendResult>, _), _> = self
            .client
            .call(Method::POST, &path, &[], Some(&body(email)))
            .await;
        let result = res.map_err(|e| explain(&e))?.0.unwrap_or_default();
        if !result.permanent_bounces.is_empty() {
            return Err(SendError {
                status: Some(200),
                message: format!("{} bounced permanently", email.to),
            });
        }
        if !result.suppressed_recipients.is_empty() {
            return Err(SendError {
                status: Some(200),
                message: format!(
                    "{} is on the account's suppression list (an earlier bounce or complaint)",
                    email.to
                ),
            });
        }
        Ok(Sent {
            message_id: result.message_id,
        })
    }
}

/// A sending domain onboarded on a zone.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SendingDomain {
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
}

/// Whether email can go out from `from` with this token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Readiness {
    Ready,
    /// The domain's zone isn't in the token's reach.
    NoZone,
    /// The token lacks Email Sending permission.
    MissingPermission,
    /// The zone has no enabled sending domain covering `from`.
    NotOnboarded,
}

/// The domain part of an address, lowercased.
pub fn domain_of(address: &str) -> Option<String> {
    let (local, domain) = address.trim().rsplit_once('@')?;
    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return None;
    }
    Some(domain.to_ascii_lowercase())
}

/// A basic address check (`local@domain.tld`, no spaces or angle brackets).
pub fn valid_address(address: &str) -> bool {
    let a = address.trim();
    a.len() <= 254
        && !a.chars().any(|c| c.is_whitespace() || "<>,;\"".contains(c))
        && a.matches('@').count() == 1
        && domain_of(a).is_some()
}

/// Whether one of `domains` sends for `domain`: the onboarded name itself,
/// or a leftmost wildcard (`*.example.com`) covering it.
pub fn covers(domains: &[SendingDomain], domain: &str) -> bool {
    let domain = domain.to_ascii_lowercase();
    domains.iter().filter(|d| d.enabled).any(|d| {
        let name = d.name.trim_end_matches('.').to_ascii_lowercase();
        match name.strip_prefix("*.") {
            Some(base) => domain.ends_with(&format!(".{base}")),
            None => name == domain,
        }
    })
}

/// Checks the token and onboarding for sending from `from`.
pub async fn readiness(client: &Client, from: &str) -> client::Result<Readiness> {
    let Some(domain) = domain_of(from) else {
        return Ok(Readiness::NotOnboarded);
    };
    let Some(zone) = client.find_zone(&domain).await? else {
        return Ok(Readiness::NoZone);
    };
    let path = format!("zones/{}/email/sending/subdomains", zone.id);
    match client
        .call::<Vec<SendingDomain>>(Method::GET, &path, &[], None)
        .await
    {
        Ok((list, _)) if covers(list.as_deref().unwrap_or_default(), &domain) => {
            Ok(Readiness::Ready)
        }
        Ok(_) => Ok(Readiness::NotOnboarded),
        Err(e) if e.is_forbidden() => Ok(Readiness::MissingPermission),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_matches_the_rest_api() {
        let e = Email {
            from: "alerts@example.com".into(),
            from_name: "Modula".into(),
            to: "me@gmail.com".into(),
            subject: "s".into(),
            text: "t".into(),
            html: "<p>t</p>".into(),
        };
        assert_eq!(
            body(&e),
            json!({
                "from": { "address": "alerts@example.com", "name": "Modula" },
                "to": ["me@gmail.com"], "subject": "s", "text": "t", "html": "<p>t</p>",
            })
        );
    }

    #[test]
    fn addresses() {
        assert!(valid_address("a@b.co"));
        assert!(valid_address(" alerts@mail.example.com "));
        assert!(!valid_address("a@b"));
        assert!(!valid_address("@b.co"));
        assert!(!valid_address("a b@c.co"));
        assert!(!valid_address("Name <a@b.co>"));
        assert!(!valid_address("a@b.co,c@d.co"));
        assert_eq!(
            domain_of("x@Mail.Example.com").as_deref(),
            Some("mail.example.com")
        );
        let d = |n: &str, enabled| SendingDomain {
            name: n.into(),
            enabled,
        };
        assert!(covers(&[d("example.com", true)], "example.com"));
        assert!(!covers(&[d("example.com", false)], "example.com"));
        assert!(!covers(&[d("mail.example.com", true)], "example.com"));
        assert!(covers(&[d("*.example.com", true)], "mail.example.com"));
        assert!(!covers(&[d("*.example.com", true)], "example.com"));
        assert!(!covers(&[d("*.example.com", true)], "badexample.com"));
    }

    #[test]
    fn errors_name_the_fix() {
        let api = |status, code: &str| client::Error::Api {
            status,
            message: format!("email.sending.error.x [{code}]"),
        };
        assert!(
            explain(&api(403, "10102"))
                .message
                .contains("Email Sending · Edit")
        );
        assert!(
            explain(&api(403, "10208"))
                .message
                .contains("verified destination")
        );
        assert!(explain(&api(429, "10004")).message.contains("quota"));
        assert_eq!(explain(&api(403, "10105")).status, Some(403));
    }
}
