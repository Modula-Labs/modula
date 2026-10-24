//! Cloudflare, connected per workspace with a scoped API token.
//!
//! "Cloudflare-native, not Cloudflare-dependent": everything here sits behind
//! a provider interface (DNS: `domains::provider::DnsProvider`; backups: a
//! plain S3 target), so a workspace without Cloudflare loses nothing but the
//! automation.
//!
//! What a connection enables:
//! - DNS records for custom domains are written automatically when the
//!   domain's zone is in the account, optionally proxied (orange cloud);
//! - a one-step "Use Cloudflare R2" backup target;
//! - R2 buckets on the project canvas, each with its own bucket-scoped
//!   credentials (see `r2` and `api::buckets`).
//!
//! The token lives in `integrations` (kind `cloudflare`), sealed with the
//! workspace data key (AAD `integrations/secret/<id>`), and is never returned
//! to clients: the API shows its last four characters only.
//!
//! TODO: proxied mode does not lock the origin yet. Still to build:
//! firewalling the edge to Cloudflare's IP ranges, Authenticated Origin Pulls
//! (mTLS from Cloudflare to Caddy) and Cloudflare Tunnel ingress for apps. The
//! dashboard itself can go through a tunnel: see `crate::private`.

pub mod api;
pub mod client;
pub mod dns;
pub mod r2;
pub mod zero_trust;

use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub use client::Client;

/// `integrations.kind` for Cloudflare.
pub const KIND: &str = "cloudflare";

/// The permissions the token needs, as shown next to the template link.
pub const PERMISSIONS: &[(&str, &str, bool)] = &[
    (
        "Zone · Zone · Read",
        "find the zone a domain lives in",
        true,
    ),
    (
        "Zone · DNS · Edit",
        "create and remove records for your domains",
        true,
    ),
    (
        "Zone · Zone Settings · Read",
        "warn when the SSL mode breaks proxied domains",
        true,
    ),
    (
        "Account · Workers R2 Storage · Edit",
        "create R2 buckets for backups and projects (optional)",
        false,
    ),
    (
        "Account · Account API Tokens · Edit",
        "give each R2 bucket its own credentials (optional)",
        false,
    ),
    (
        "Account · Cloudflare Tunnel · Edit",
        "run the dashboard through a tunnel (for private mode)",
        false,
    ),
    (
        "Account · Access: Apps and Policies · Edit",
        "put Cloudflare Access in front of the dashboard (for private mode)",
        false,
    ),
];

/// Cloudflare's dashboard with a new user API token pre-filled with exactly
/// [`PERMISSIONS`] for all zones and accounts (the user can narrow both).
pub fn token_template_url() -> String {
    let keys = r#"[{"key":"zone","type":"read"},{"key":"dns","type":"edit"},{"key":"zone_settings","type":"read"},{"key":"workers_r2","type":"edit"},{"key":"account_api_tokens","type":"edit"},{"key":"argotunnel","type":"edit"},{"key":"access","type":"edit"}]"#;
    let mut url =
        url::Url::parse("https://dash.cloudflare.com/profile/api-tokens").expect("static");
    url.query_pairs_mut()
        .append_pair("permissionGroupKeys", keys)
        .append_pair("accountId", "*")
        .append_pair("zoneId", "all")
        .append_pair("name", "Modula Deploy");
    url.into()
}

/// The S3 endpoint for an account's R2 buckets.
pub fn r2_endpoint(account_id: &str) -> String {
    format!("https://{account_id}.r2.cloudflarestorage.com")
}

pub fn secret_aad(workspace: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: workspace,
        table: "integrations",
        column: "secret",
        row,
    }
}

/// Non-secret details kept in `integrations.metadata`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    /// The token's last four characters, to recognise it.
    pub token_last4: String,
    /// Cloudflare's id for the token (not a secret).
    pub token_id: String,
}

/// A workspace's live connection.
pub struct Connection {
    pub id: Id,
    pub workspace: Id,
    pub metadata: Metadata,
    pub client: Client,
}

/// Validates a pasted API token (shape only; Cloudflare decides the rest).
pub fn parse_token(raw: &str) -> AppResult<SecretString> {
    let t = raw.trim();
    if t.len() < 20 || t.len() > 255 || !t.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AppError::bad_request(
            "that doesn't look like a Cloudflare API token",
        ));
    }
    Ok(SecretString::from(t.to_owned()))
}

pub fn last4(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

/// The workspace's Cloudflare connection, with its token unsealed.
pub async fn connection(state: &AppState, workspace: Id) -> AppResult<Option<Connection>> {
    let row: Option<(Id, Vec<u8>, serde_json::Value)> = sqlx::query_as(
        "SELECT id, secret_sealed, metadata FROM integrations
         WHERE workspace_id = $1 AND kind = 'cloudflare'",
    )
    .bind(workspace)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, sealed, metadata)) = row else {
        return Ok(None);
    };
    let row = id.to_string();
    let plain = state
        .keys
        .open(&state.db, &sealed, secret_aad(workspace, &row))
        .await?;
    let token = std::str::from_utf8(&plain)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("stored Cloudflare token: {e}")))?;
    Ok(Some(Connection {
        id,
        workspace,
        metadata: serde_json::from_value(metadata).unwrap_or_default(),
        client: Client::new(
            state.cfg.cloudflare_api.clone(),
            SecretString::from(token.to_owned()),
        ),
    }))
}

/// Maps a Cloudflare failure to an error safe to show the user.
pub fn upstream(e: client::Error) -> AppError {
    match e {
        client::Error::Api { .. } => AppError::Unavailable(e.to_string()),
        other => {
            tracing::warn!(error = %other, "Cloudflare API call failed");
            AppError::Unavailable(other.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_url_carries_the_permissions() {
        let u = url::Url::parse(&token_template_url()).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        let keys: serde_json::Value = serde_json::from_str(&q["permissionGroupKeys"]).unwrap();
        let keys: Vec<String> = keys
            .as_array()
            .unwrap()
            .iter()
            .map(|k| {
                format!(
                    "{}:{}",
                    k["key"].as_str().unwrap(),
                    k["type"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(
            keys,
            [
                "zone:read",
                "dns:edit",
                "zone_settings:read",
                "workers_r2:edit",
                "account_api_tokens:edit",
                "argotunnel:edit",
                "access:edit"
            ]
        );
        assert_eq!(q["accountId"], "*");
        assert_eq!(q["zoneId"], "all");
    }

    #[test]
    fn tokens() {
        assert!(parse_token("short").is_err());
        assert!(parse_token("has space in the middle of it!!").is_err());
        assert!(parse_token("  abcdefghijklmnopqrstuvwxyz0123456789_-  ").is_ok());
        assert_eq!(last4("abcdefgh"), "efgh");
        assert_eq!(last4("ab"), "ab");
    }
}
