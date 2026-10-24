//! Hetzner Cloud: a workspace's API token (sealed), provisioning caps, and
//! the [`ServerProvider`] implementation.

pub mod api;
pub mod client;
pub mod firewall;

use std::collections::BTreeMap;

use futures::FutureExt;
use futures::future::BoxFuture;
use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sqlx::PgExecutor;

use self::client::{Client, CreateServer, FirewallRef, FirewallRule};
use crate::error::AppResult;
use crate::locations;
use crate::provision::{
    CreatedServer, LocationPrice, NewServer, Phase, Power, ProviderError, ProviderLocation,
    ServerProvider, ServerState, ServerTypeOffer,
};
use crate::state::AppState;

pub const KIND: &str = "hetzner";
pub const IMAGE: &str = "ubuntu-24.04";
/// Where users create a token (Project → Security → API tokens).
pub const TOKEN_HELP_URL: &str = "https://console.hetzner.cloud/projects";

/// Spending guards, stored in `integrations.metadata`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    pub max_servers: i64,
    /// Gross euro cents per month, across all servers created here.
    pub max_monthly_cents: i64,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_servers: 5,
            max_monthly_cents: 10_000,
        }
    }
}

impl Caps {
    pub const MAX_SERVERS: i64 = 100;
    pub const MAX_MONTHLY_CENTS: i64 = 10_000_000;

    pub fn from_metadata(v: &serde_json::Value) -> Self {
        let d = Self::default();
        Self {
            max_servers: v["maxServers"].as_i64().unwrap_or(d.max_servers),
            max_monthly_cents: v["maxMonthlyCents"].as_i64().unwrap_or(d.max_monthly_cents),
        }
    }
}

/// What the workspace has provisioned so far (counts every row: a failed
/// server may still exist, and bill, until it is deleted).
#[derive(Debug, Clone, Copy, Serialize, Default, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub servers: i64,
    pub monthly_cents: i64,
}

pub async fn usage<'e>(db: impl PgExecutor<'e>, workspace: Id) -> AppResult<Usage> {
    Ok(sqlx::query_as(
        "SELECT count(*) AS servers, COALESCE(sum(monthly_price_cents), 0)::bigint AS monthly_cents
         FROM provisioned_servers WHERE workspace_id = $1 AND provider = 'hetzner'",
    )
    .bind(workspace)
    .fetch_one(db)
    .await?)
}

pub(crate) fn aad(workspace: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: workspace,
        table: "integrations",
        column: "secret",
        row,
    }
}

/// The workspace's Hetzner provider, or None when not connected.
pub async fn provider(state: &AppState, workspace: Id) -> AppResult<Option<Hetzner>> {
    let row: Option<(Id, Vec<u8>)> = sqlx::query_as(
        "SELECT id, secret_sealed FROM integrations WHERE workspace_id = $1 AND kind = 'hetzner'",
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
    let token = String::from_utf8(plain.to_vec())
        .map_err(|_| anyhow::anyhow!("stored Hetzner token is not UTF-8"))?;
    let mut h = Hetzner::new(state, workspace, token.into())?;
    h.mesh_sources = firewall::mesh_sources(&state.db, Some(workspace)).await?;
    Ok(Some(h))
}

pub struct Hetzner {
    client: Client,
    workspace: Id,
    /// Public IPs allowed to the WireGuard port ([`firewall::mesh_sources`]).
    mesh_sources: Vec<String>,
}

impl Hetzner {
    pub fn new(
        state: &AppState,
        workspace: Id,
        token: SecretString,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            client: Client::new(state.cfg.hetzner_api.clone(), token)?,
            workspace,
            mesh_sources: Vec::new(),
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn workspace(&self) -> Id {
        self.workspace
    }

    /// One firewall per workspace and SSH choice, shared by its servers;
    /// its rules are reset to ours on every use.
    async fn ensure_firewall(&self, ssh: bool) -> Result<i64, ProviderError> {
        let ssh_label = if ssh { "1" } else { "0" };
        let selector = format!("modula=1,workspace={},ssh={ssh_label}", self.workspace);
        let rules = firewall_rules(ssh, &self.mesh_sources);
        if let Some(fw) = self.client.firewalls(&selector).await?.into_iter().next() {
            self.client.set_firewall_rules(fw.id, &rules).await?;
            return Ok(fw.id);
        }
        let labels = BTreeMap::from([
            ("modula".to_owned(), "1".to_owned()),
            ("workspace".to_owned(), self.workspace.to_string()),
            ("ssh".to_owned(), ssh_label.to_owned()),
        ]);
        let name = format!("modula-{}{}", self.workspace, if ssh { "-ssh" } else { "" });
        Ok(self
            .client
            .create_firewall(&name, &labels, &rules)
            .await?
            .id)
    }
}

/// Inbound only what a worker serves: HTTP(S) (incl. HTTP/3), the TCP proxy
/// range, ping, SSH when asked, and WireGuard from Modula's other servers
/// ([`firewall::mesh_rule`]). Workers dial out to the control plane, so
/// nothing else is needed; outbound stays open.
pub fn firewall_rules(ssh: bool, mesh_sources: &[String]) -> Vec<FirewallRule> {
    let rule = FirewallRule::inbound_any;
    let mut rules = vec![
        rule("tcp", Some("80"), "HTTP"),
        rule("tcp", Some("443"), "HTTPS"),
        rule("udp", Some("443"), "HTTP/3"),
        rule("tcp", Some("20000-29999"), "Modula TCP proxies"),
        rule("icmp", None, "Ping"),
    ];
    if ssh {
        rules.push(rule("tcp", Some("22"), "SSH"));
    }
    rules.extend(firewall::mesh_rule(mesh_sources));
    rules
}

fn parse_id(id: &str) -> Result<i64, ProviderError> {
    id.parse().map_err(|_| ProviderError::NotFound)
}

impl ServerProvider for Hetzner {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn locations(&self) -> BoxFuture<'_, Result<Vec<ProviderLocation>, ProviderError>> {
        async move {
            let list = self.client.locations().await?;
            Ok(list
                .into_iter()
                .filter_map(|l| {
                    let location = locations::resolve(KIND, &l.name, "")?;
                    (location.provider == KIND).then_some(ProviderLocation {
                        region: l.name,
                        location,
                    })
                })
                .collect())
        }
        .boxed()
    }

    fn server_types(&self) -> BoxFuture<'_, Result<Vec<ServerTypeOffer>, ProviderError>> {
        async move {
            let types = self.client.server_types().await?;
            let mut offers: Vec<ServerTypeOffer> = types
                .into_iter()
                .filter(|t| !t.is_deprecated())
                .filter_map(|t| {
                    let prices: Vec<LocationPrice> = t
                        .prices
                        .iter()
                        .filter(|p| t.orderable_in(&p.location))
                        .filter_map(|p| {
                            let location = locations::resolve(KIND, &p.location, "")?;
                            Some(LocationPrice {
                                location: location.id,
                                monthly_cents: client::price_cents(&p.price_monthly.gross)?,
                            })
                        })
                        .collect();
                    (!prices.is_empty()).then_some(ServerTypeOffer {
                        name: t.name,
                        description: t.description,
                        cores: t.cores,
                        memory_gb: t.memory,
                        disk_gb: t.disk,
                        arch: t.architecture,
                        cpu_type: t.cpu_type,
                        prices,
                    })
                })
                .collect();
            offers.sort_by_key(|o| {
                (
                    o.prices
                        .iter()
                        .map(|p| p.monthly_cents)
                        .min()
                        .unwrap_or(i64::MAX),
                    o.name.clone(),
                )
            });
            Ok(offers)
        }
        .boxed()
    }

    fn create<'a>(
        &'a self,
        req: &'a NewServer,
    ) -> BoxFuture<'a, Result<CreatedServer, ProviderError>> {
        async move {
            let firewall = self.ensure_firewall(req.ssh).await?;
            let body = CreateServer {
                name: req.name.clone(),
                server_type: req.server_type.clone(),
                location: req.region.clone(),
                image: IMAGE.to_owned(),
                ssh_keys: req.ssh_key_ids.clone(),
                user_data: req.user_data.clone(),
                labels: req.labels.clone(),
                firewalls: vec![FirewallRef { firewall }],
                start_after_create: true,
            };
            let (server, action) = self.client.create_server(&body).await?;
            Ok(CreatedServer {
                ipv4: server.ipv4(),
                id: server.id.to_string(),
                firewall_id: Some(firewall.to_string()),
                action_id: action.map(|a| a.to_string()),
            })
        }
        .boxed()
    }

    fn find<'a>(
        &'a self,
        modula_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<CreatedServer>, ProviderError>> {
        async move {
            let selector = format!("modula-id={modula_id}");
            let servers = self.client.servers(&selector).await?;
            Ok(servers.into_iter().next().map(|s| CreatedServer {
                ipv4: s.ipv4(),
                id: s.id.to_string(),
                firewall_id: None,
                action_id: None,
            }))
        }
        .boxed()
    }

    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), ProviderError>> {
        async move { self.client.delete_server(parse_id(id)?).await }.boxed()
    }

    fn power<'a>(&'a self, id: &'a str, op: Power) -> BoxFuture<'a, Result<(), ProviderError>> {
        async move {
            let action = match op {
                Power::On => "poweron",
                // A clean ACPI shutdown; `poweroff` would cut power.
                Power::Off => "shutdown",
                Power::Reboot => "reboot",
            };
            self.client.server_action(parse_id(id)?, action).await?;
            Ok(())
        }
        .boxed()
    }

    fn status<'a>(
        &'a self,
        id: &'a str,
        action_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ServerState, ProviderError>> {
        async move {
            let server = match self.client.server(parse_id(id)?).await {
                Ok(s) => s,
                Err(ProviderError::NotFound) => {
                    return Ok(ServerState {
                        phase: Phase::Gone,
                        ipv4: None,
                        error: None,
                    });
                }
                Err(e) => return Err(e),
            };
            let mut error = None;
            if let Some(a) = action_id.and_then(|a| a.parse::<i64>().ok()) {
                let action = self.client.action(a).await?;
                if action.status == "error" {
                    error = Some(
                        action
                            .error
                            .map(|e| format!("{} ({})", e.message, e.code))
                            .unwrap_or_else(|| "server creation failed".into()),
                    );
                }
            }
            let phase = match server.status.as_str() {
                "running" => Phase::Running,
                "off" | "stopping" => Phase::Off,
                "deleting" => Phase::Gone,
                _ => Phase::Booting,
            };
            Ok(ServerState {
                phase,
                ipv4: server.ipv4(),
                error,
            })
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_opens_only_web_and_proxies() {
        let ports = |ssh, mesh: &[String]| {
            firewall_rules(ssh, mesh)
                .into_iter()
                .map(|r| format!("{}/{}", r.protocol, r.port.as_deref().unwrap_or("-")))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ports(false, &[]),
            ["tcp/80", "tcp/443", "udp/443", "tcp/20000-29999", "icmp/-"]
        );
        assert!(ports(true, &[]).contains(&"tcp/22".to_owned()));
        assert!(
            firewall_rules(true, &[])
                .iter()
                .all(|r| r.direction == "in")
        );
        // WireGuard only from the other servers.
        let rules = firewall_rules(false, &["203.0.113.4".to_owned()]);
        let wg = rules.last().unwrap();
        assert_eq!(
            (wg.protocol.as_str(), wg.port.as_deref()),
            ("udp", Some("51820"))
        );
        assert_eq!(wg.source_ips, ["203.0.113.4/32"]);
    }

    #[test]
    fn caps_default_and_parse() {
        assert_eq!(Caps::from_metadata(&serde_json::json!({})), Caps::default());
        let c = Caps::from_metadata(&serde_json::json!({"maxServers": 2, "maxMonthlyCents": 999}));
        assert_eq!((c.max_servers, c.max_monthly_cents), (2, 999));
    }
}
