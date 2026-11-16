//! AWS EC2: a workspace's access key (sealed) and provisioning caps. The
//! [`ServerProvider`](crate::provision::ServerProvider) implementation lives
//! in `modula-ec2` beside the client, so the `modula` CLI can use it too.

pub mod api;

use modula_crypto::envelope::FieldAad;
use modula_ec2::provider::Ec2Provider;
use modula_ec2::{Client, Credentials};
use modula_id::Id;
use secrecy::{ExposeSecret, SecretString};

use crate::error::AppResult;
use crate::provision::ProviderError;
use crate::provision::mesh::mesh_sources;
use crate::state::AppState;

pub use modula_ec2::provider::KIND;

/// Where users create an access key.
pub const KEY_HELP_URL: &str = "https://console.aws.amazon.com/iam/home#/users";

pub(crate) fn aad(workspace: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: workspace,
        table: "integrations",
        column: "secret",
        row,
    }
}

pub(crate) fn client(state: &AppState, creds: Credentials) -> Result<Client, ProviderError> {
    Client::new(state.cfg.aws_api.clone(), creds)
}

/// The sealed form: both halves of the key together.
pub(crate) fn encode_credentials(access_key_id: &str, secret: &SecretString) -> Vec<u8> {
    serde_json::json!({
        "accessKeyId": access_key_id,
        "secretAccessKey": secret.expose_secret(),
    })
    .to_string()
    .into_bytes()
}

pub(crate) fn decode_credentials(plain: &[u8]) -> anyhow::Result<Credentials> {
    let v: serde_json::Value = serde_json::from_slice(plain)
        .map_err(|_| anyhow::anyhow!("stored AWS credentials are not valid"))?;
    let field = |name: &str| {
        v[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("stored AWS credentials are not valid"))
    };
    Ok(Credentials {
        access_key_id: field("accessKeyId")?,
        secret_access_key: SecretString::from(field("secretAccessKey")?),
    })
}

/// The workspace's AWS provider, or None when not connected.
pub async fn provider(state: &AppState, workspace: Id) -> AppResult<Option<Ec2Provider>> {
    let row: Option<(Id, Vec<u8>)> = sqlx::query_as(
        "SELECT id, secret_sealed FROM integrations WHERE workspace_id = $1 AND kind = 'aws'",
    )
    .bind(workspace)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, sealed)) = row else {
        return Ok(None);
    };
    let row = id.to_string();
    let plain = state
        .keys
        .open(&state.db, &sealed, aad(workspace, &row))
        .await?;
    let client = client(state, decode_credentials(&plain)?)?;
    let sources = mesh_sources(&state.db, Some(workspace)).await?;
    Ok(Some(Ec2Provider::new(
        client,
        workspace.to_string(),
        sources,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::hetzner;
    use crate::provision::cloud_init::Bootstrap;
    use modula_ec2::firewall;
    use modula_ec2::provider::{USER_DATA_LIMIT, user_data_for};

    #[test]
    fn credentials_round_trip() {
        let sealed = encode_credentials("AKIAEXAMPLE", &SecretString::from("sec\"ret"));
        let c = decode_credentials(&sealed).unwrap();
        assert_eq!(c.access_key_id, "AKIAEXAMPLE");
        assert_eq!(c.secret_access_key.expose_secret(), "sec\"ret");
    }

    #[test]
    fn the_guide_shows_the_policy_the_code_needs() {
        let guide = include_str!("../../../../../guide/integrations.md");
        assert!(guide.contains(modula_ec2::MINIMAL_IAM_POLICY));
    }

    #[test]
    fn firewall_matches_hetzners_ports() {
        let summary = |ssh| {
            let mut ours: Vec<String> = firewall::worker_rules(ssh)
                .into_iter()
                .map(|r| {
                    if r.from == r.to && r.from >= 0 {
                        format!("{}/{}", r.protocol, r.from)
                    } else if r.from < 0 {
                        format!("{}/-", r.protocol)
                    } else {
                        format!("{}/{}-{}", r.protocol, r.from, r.to)
                    }
                })
                .collect();
            ours.sort();
            let mut theirs: Vec<String> = hetzner::firewall_rules(ssh, &[])
                .into_iter()
                .map(|r| format!("{}/{}", r.protocol, r.port.as_deref().unwrap_or("-")))
                .collect();
            theirs.sort();
            assert_eq!(ours, theirs);
        };
        summary(false);
        summary(true);
        let mesh: Vec<String> = hetzner::main_firewall_rules(7101, &[])
            .into_iter()
            .map(|r| format!("{}/{}", r.protocol, r.port.as_deref().unwrap_or("-")))
            .collect();
        for r in firewall::main_rules(7101) {
            let port = if r.from < 0 {
                "-".to_owned()
            } else if r.from == r.to {
                r.from.to_string()
            } else {
                format!("{}-{}", r.from, r.to)
            };
            assert!(mesh.contains(&format!("{}/{port}", r.protocol)));
        }
    }

    #[test]
    fn user_data_fits_the_16_kb_limit() {
        // A long dashboard URL and token, and the nine extra RSA keys the
        // key limit allows beyond the launch key.
        let token = format!("mcj_{}", "x".repeat(60));
        let ca = format!("sha256:{}", "0".repeat(64));
        let name = "a".repeat(63);
        let origin = format!("https://{}.example.com", "d".repeat(60));
        let script = Bootstrap {
            install_url: format!("{origin}/api/install.sh"),
            server: format!("{origin}:7101"),
            token: &token,
            ca: &ca,
            name: &name,
        }
        .render();
        let rsa = format!("ssh-rsa {} user@some-long-hostname", "A".repeat(716));
        let extra = vec![rsa; 9];
        let data = user_data_for(&script, "203.0.113.200", &extra);
        assert!(
            data.len() <= USER_DATA_LIMIT,
            "user data is {} bytes",
            data.len()
        );
    }
}
