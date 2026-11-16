//! Which cloud `server create`, `options` and `resize` talk to. Everything
//! common goes through [`ServerProvider`]; [`Cloud`] adds the few steps only
//! the CLI needs (a main server's firewall on Hetzner, uploading an SSH key,
//! cleaning up after a failure), so the flows don't name a provider.
//!
//! EC2 uses the provider shared with the control plane. Hetzner's lives in
//! the server, tied to its database, so [`HetznerCloud`] is the CLI's own
//! thin one on the shared client: it differs in `create`, which attaches the
//! main server's firewall made beforehand instead of a workspace's.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use futures::FutureExt;
use futures::future::BoxFuture;
use modula_ec2::provider::Ec2Provider;
use modula_ec2::{Client as Ec2Client, Credentials};
use modula_hetzner::{Client, CreateServer, FirewallRef, FirewallRule, Server, ServerType};
use modula_provider::{
    ActionState, CreatedServer, DEFAULT_EUR_USD_RATE, LocationPrice, MeshSources, NewServer, Phase,
    Power, ProviderError, ProviderLocation, ProviderServer, ProviderSshKey, ServerProvider,
    ServerState, ServerTypeOffer, eur_cents_to_usd, locations,
};
use secrecy::{ExposeSecret, SecretString};

use super::flow::{IMAGE, Settings};
use crate::cli::Provider;

/// The control plane's node port (MODULA_NODE_LISTEN in install.sh).
pub const NODE_PORT: u16 = 7101;

impl Provider {
    /// The id used in `--json` and the instance's API.
    pub fn id(self) -> &'static str {
        match self {
            Self::Hetzner => "hetzner",
            Self::Aws => "aws",
        }
    }

    /// Its name in sentences.
    pub fn label(self) -> &'static str {
        match self {
            Self::Hetzner => "Hetzner",
            Self::Aws => "AWS",
        }
    }

    /// Who to delete leftovers with, and what that takes.
    pub fn console(self) -> &'static str {
        match self {
            Self::Hetzner => "the Hetzner console",
            Self::Aws => "the AWS console (terminate the instance and release its Elastic IP)",
        }
    }

    /// The account an SSH key logs in to.
    pub fn ssh_user(self) -> &'static str {
        match self {
            Self::Hetzner => "root",
            Self::Aws => "ubuntu",
        }
    }

    /// Hetzner bills in euros, so its dollar prices are approximate.
    pub fn approximate_prices(self) -> bool {
        matches!(self, Self::Hetzner)
    }

    /// Where a server is created when nobody chose.
    pub fn default_location(self) -> &'static str {
        match self {
            Self::Hetzner => "fsn1",
            Self::Aws => "us-east-1",
        }
    }

    /// How the prices shown are made up.
    pub fn price_note(self) -> String {
        match self {
            Self::Hetzner => format!(
                "Prices are approximate: Hetzner bills in euros (incl. VAT), shown at {DEFAULT_EUR_USD_RATE} USD per EUR."
            ),
            Self::Aws => {
                "Monthly prices: 730 hours of compute, an 80 GB gp3 root disk and the Elastic IP (before tax)."
                    .to_owned()
            }
        }
    }
}

/// An AWS access key.
#[derive(Clone)]
pub struct AwsKeys {
    pub access_key_id: String,
    pub secret_access_key: SecretString,
}

/// The credentials of one cloud.
#[derive(Clone)]
pub enum CloudSecret {
    Hetzner(SecretString),
    Aws(AwsKeys),
}

impl CloudSecret {
    pub fn provider(&self) -> Provider {
        match self {
            Self::Hetzner(_) => Provider::Hetzner,
            Self::Aws(_) => Provider::Aws,
        }
    }
}

/// A provider's errors, worded for this command.
pub fn error(kind: Provider, e: ProviderError) -> anyhow::Error {
    match (kind, e) {
        (Provider::Hetzner, ProviderError::Unauthorized) => anyhow::anyhow!(
            "Hetzner rejected the API token (create one under Security → API tokens in your Hetzner Cloud project)"
        ),
        (Provider::Hetzner, ProviderError::Forbidden) => {
            anyhow::anyhow!("the Hetzner API token is read-only; create one with Read & Write")
        }
        (Provider::Aws, ProviderError::Unauthorized) => anyhow::anyhow!(
            "AWS rejected the access key (check AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY)"
        ),
        (Provider::Aws, ProviderError::Forbidden) => anyhow::anyhow!(
            "the AWS access key lacks permissions; attach this IAM policy to its user:\n{}",
            modula_ec2::MINIMAL_IAM_POLICY
        ),
        (kind, other) => anyhow::anyhow!("{}: {other}", kind.label()),
    }
}

/// A firewall ready for the server to attach.
pub struct PreparedFirewall {
    pub id: String,
    pub name: String,
    /// Left from an earlier attempt with this name: it stays on cleanup.
    pub reused: bool,
}

/// One connected cloud.
pub enum Cloud {
    Hetzner(HetznerCloud),
    Aws(Ec2Provider),
}

impl Cloud {
    /// `workspace` tags the EC2 security groups a server starts with
    /// (`modula-<workspace>…`); it must not start with `main-`, which would
    /// collide with a main server's own group names.
    pub fn connect(
        secret: &CloudSecret,
        settings: &Settings,
        workspace: &str,
    ) -> anyhow::Result<Self> {
        Ok(match secret {
            CloudSecret::Hetzner(token) => Self::Hetzner(HetznerCloud::new(
                Client::new(settings.hetzner_api.clone(), token.clone())
                    .map_err(|e| anyhow::anyhow!("Hetzner: {e}"))?,
            )),
            CloudSecret::Aws(keys) => {
                let client = Ec2Client::new(
                    settings.aws_api.clone(),
                    Credentials {
                        access_key_id: keys.access_key_id.clone(),
                        secret_access_key: keys.secret_access_key.clone(),
                    },
                )
                .map_err(|e| anyhow::anyhow!("AWS: {e}"))?;
                Self::Aws(Ec2Provider::new(client, workspace, Vec::new()))
            }
        })
    }

    pub fn kind(&self) -> Provider {
        match self {
            Self::Hetzner(_) => Provider::Hetzner,
            Self::Aws(_) => Provider::Aws,
        }
    }

    pub fn provider(&self) -> &dyn ServerProvider {
        match self {
            Self::Hetzner(h) => h,
            Self::Aws(a) => a,
        }
    }

    /// Hetzner attaches the firewall when it creates the server, so it must
    /// exist first; EC2's is set once the instance exists
    /// ([`ServerProvider::open_node_port`]).
    pub fn firewall_before_server(&self) -> bool {
        matches!(self, Self::Hetzner(_))
    }

    /// The firewall `name`'s main server will attach, when
    /// [`Self::firewall_before_server`].
    pub async fn prepare_firewall(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> anyhow::Result<PreparedFirewall> {
        let Self::Hetzner(h) = self else {
            anyhow::bail!("this provider sets the firewall after creating the server");
        };
        h.prepare_firewall(name, labels)
            .await
            .map_err(|e| error(Provider::Hetzner, e))
    }

    /// Put a local public key at the provider (AWS: in `region`).
    pub async fn upload_ssh_key(
        &self,
        region: &str,
        name: &str,
        public_key: &str,
    ) -> Result<ProviderSshKey, ProviderError> {
        match self {
            Self::Hetzner(h) => {
                let k = h
                    .client
                    .create_ssh_key(name, public_key, &BTreeMap::new())
                    .await?;
                Ok(ProviderSshKey {
                    id: k.id.to_string(),
                    name: k.name,
                    fingerprint: k.fingerprint,
                    public_key: Some(k.public_key),
                })
            }
            Self::Aws(a) => {
                a.client().import_key(region, name, public_key).await?;
                Ok(ProviderSshKey {
                    id: format!("{region}/{name}"),
                    name: name.to_owned(),
                    fingerprint: String::new(),
                    public_key: Some(public_key.to_owned()),
                })
            }
        }
    }

    /// Delete a firewall made by [`Self::prepare_firewall`].
    pub async fn delete_firewall(&self, id: &str) -> Result<(), ProviderError> {
        match self {
            Self::Hetzner(h) => h.client.delete_firewall(parse_id(id)?).await,
            Self::Aws(_) => Ok(()),
        }
    }
}

/// Inbound what a main server serves, as install.sh opens it in ufw: SSH,
/// HTTP(S) incl. HTTP/3, the TCP proxy range, the node port, and ping.
pub fn firewall_rules() -> Vec<FirewallRule> {
    let rule = FirewallRule::inbound_any;
    vec![
        rule("tcp", Some("22"), "SSH"),
        rule("tcp", Some("80"), "HTTP"),
        rule("tcp", Some("443"), "HTTPS"),
        rule("udp", Some("443"), "HTTP/3"),
        rule("tcp", Some("20000-29999"), "Modula TCP proxies"),
        rule("tcp", Some(&NODE_PORT.to_string()), "Modula nodes"),
        rule("icmp", None, "Ping"),
    ]
}

fn parse_id(id: &str) -> Result<i64, ProviderError> {
    id.parse().map_err(|_| ProviderError::NotFound)
}

fn phase(status: &str) -> Phase {
    match status {
        "running" => Phase::Running,
        "stopping" => Phase::Stopping,
        "off" => Phase::Off,
        "deleting" => Phase::Gone,
        _ => Phase::Booting,
    }
}

fn describe_server(s: &Server) -> ProviderServer {
    let ty = s.server_type.as_ref();
    ProviderServer {
        id: s.id.to_string(),
        name: s.name.clone(),
        ipv4: s.ipv4(),
        phase: phase(&s.status),
        location: s
            .location
            .as_ref()
            .and_then(|l| locations::resolve("hetzner", &l.name, ""))
            .filter(|l| l.provider == "hetzner"),
        server_type: ty.map(|t| t.name.clone()).unwrap_or_default(),
        cores: ty.map_or(0, |t| t.cores),
        memory_gb: ty.map_or(0.0, |t| t.memory),
        disk_gb: s.disk_gb().unwrap_or(0),
        arch: ty.map(|t| t.architecture.clone()).unwrap_or_default(),
        storage_type: ty.map(|t| t.storage_type.clone()).unwrap_or_default(),
        role: s.labels.get("role").cloned(),
    }
}

fn as_hetzner_type(name: &str, disk: u32, arch: &str, storage: &str) -> ServerType {
    ServerType {
        id: 0,
        name: name.to_owned(),
        description: String::new(),
        cores: 0,
        memory: 0.0,
        disk,
        cpu_type: String::new(),
        architecture: arch.to_owned(),
        storage_type: storage.to_owned(),
        deprecated: None,
        deprecation: None,
        prices: Vec::new(),
        locations: Vec::new(),
    }
}

/// Hetzner, as the CLI uses it.
pub struct HetznerCloud {
    client: Client,
    /// Set by [`Self::prepare_firewall`].
    firewall: OnceLock<i64>,
}

impl HetznerCloud {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            firewall: OnceLock::new(),
        }
    }

    /// Reuse ours from an earlier attempt with this name (keeping the
    /// private network's rule the control plane maintains), else create it.
    async fn prepare_firewall(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<PreparedFirewall, ProviderError> {
        let selector = format!("modula=1,role=main,server={name}");
        let existing = self.client.firewalls(&selector).await?;
        let (firewall, reused) = match existing.into_iter().next() {
            Some(fw) => {
                let mut rules = firewall_rules();
                rules.extend(
                    fw.rules
                        .iter()
                        .filter(|r| r.description.as_deref() == Some(modula_hetzner::MESH_RULE))
                        .cloned(),
                );
                self.client.set_firewall_rules(fw.id, &rules).await?;
                (fw, true)
            }
            None => (
                self.client
                    .create_firewall(&format!("modula-{name}"), labels, &firewall_rules())
                    .await?,
                false,
            ),
        };
        let _ = self.firewall.set(firewall.id);
        Ok(PreparedFirewall {
            id: firewall.id.to_string(),
            name: firewall.name,
            reused,
        })
    }

    /// Every server labelled as a Modula main server.
    pub async fn main_servers(&self) -> Result<Vec<ProviderServer>, ProviderError> {
        Ok(self
            .client
            .servers("modula=1")
            .await?
            .iter()
            .filter(|s| s.labels.get("role").map(String::as_str) == Some("main"))
            .map(describe_server)
            .collect())
    }
}

impl ServerProvider for HetznerCloud {
    fn kind(&self) -> &'static str {
        "hetzner"
    }

    fn locations(&self) -> BoxFuture<'_, Result<Vec<ProviderLocation>, ProviderError>> {
        async move {
            Ok(self
                .client
                .locations()
                .await?
                .into_iter()
                .filter_map(|l| {
                    let location = locations::resolve("hetzner", &l.name, "")?;
                    (location.provider == "hetzner").then_some(ProviderLocation {
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
            Ok(self
                .client
                .server_types()
                .await?
                .into_iter()
                .filter(|t| !t.is_deprecated())
                .filter_map(|t| {
                    let prices: Vec<LocationPrice> = t
                        .prices
                        .iter()
                        .filter(|p| t.orderable_in(&p.location))
                        .filter_map(|p| {
                            let location = locations::resolve("hetzner", &p.location, "")?;
                            Some(LocationPrice {
                                location: location.id,
                                monthly_cents: eur_cents_to_usd(
                                    modula_hetzner::price_cents(&p.price_monthly.gross)?,
                                    DEFAULT_EUR_USD_RATE,
                                ),
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
                        storage_type: t.storage_type,
                        prices,
                    })
                })
                .collect())
        }
        .boxed()
    }

    fn create<'a>(
        &'a self,
        req: &'a NewServer,
    ) -> BoxFuture<'a, Result<CreatedServer, ProviderError>> {
        async move {
            let ssh_keys = req
                .ssh_key_ids
                .iter()
                .map(|k| {
                    k.parse::<i64>().map_err(|_| ProviderError::Api {
                        code: "invalid_ssh_key".into(),
                        message: format!("{k:?} is not a Hetzner SSH key id"),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let firewall = *self.firewall.get().ok_or_else(|| ProviderError::Api {
                code: "invalid_request".into(),
                message: "the firewall wasn't prepared".into(),
            })?;
            let (server, action) = self
                .client
                .create_server(&CreateServer {
                    name: req.name.clone(),
                    server_type: req.server_type.clone(),
                    location: req.region.clone(),
                    image: IMAGE.to_owned(),
                    ssh_keys,
                    user_data: req.user_data.clone(),
                    labels: req.labels.clone(),
                    firewalls: vec![FirewallRef { firewall }],
                    start_after_create: true,
                })
                .await?;
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
            let servers = self
                .client
                .servers(&format!("modula-id={modula_id}"))
                .await?;
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
                Power::Off => "shutdown",
                Power::ForceOff => "poweroff",
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
        _action_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ServerState, ProviderError>> {
        async move {
            match self.client.server(parse_id(id)?).await {
                Ok(s) => Ok(ServerState {
                    phase: phase(&s.status),
                    ipv4: s.ipv4(),
                    error: None,
                }),
                Err(ProviderError::NotFound) => Ok(ServerState {
                    phase: Phase::Gone,
                    ipv4: None,
                    error: None,
                }),
                Err(e) => Err(e),
            }
        }
        .boxed()
    }

    fn describe<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProviderServer>, ProviderError>> {
        async move {
            match self.client.server(parse_id(id)?).await {
                Ok(s) => Ok(Some(describe_server(&s))),
                Err(ProviderError::NotFound) => Ok(None),
                Err(e) => Err(e),
            }
        }
        .boxed()
    }

    fn find_by_address<'a>(
        &'a self,
        ipv4: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProviderServer>, ProviderError>> {
        async move {
            let mut found: Vec<Server> = self
                .client
                .servers("modula=1")
                .await?
                .into_iter()
                .filter(|s| s.ipv4().as_deref() == Some(ipv4))
                .collect();
            found.sort_by_key(|s| s.labels.get("role").map(String::as_str) != Some("main"));
            Ok(found.first().map(describe_server))
        }
        .boxed()
    }

    fn resize_blocker(&self, server: &ProviderServer, to: &ServerTypeOffer) -> Option<String> {
        let current = Server {
            id: 0,
            name: server.name.clone(),
            status: String::new(),
            public_net: None,
            server_type: Some(as_hetzner_type(
                &server.server_type,
                server.disk_gb,
                &server.arch,
                &server.storage_type,
            )),
            location: None,
            labels: BTreeMap::new(),
            primary_disk_size: Some(server.disk_gb),
        };
        current.change_type_blocker(&as_hetzner_type(
            &to.name,
            to.disk_gb,
            &to.arch,
            &to.storage_type,
        ))
    }

    fn resize<'a>(
        &'a self,
        id: &'a str,
        server_type: &'a str,
    ) -> BoxFuture<'a, Result<String, ProviderError>> {
        async move {
            // Keep the disk: a bigger one could never be downsized again.
            let action = self
                .client
                .change_server_type(parse_id(id)?, server_type, false)
                .await?;
            Ok(action.id.to_string())
        }
        .boxed()
    }

    fn action<'a>(
        &'a self,
        action_id: &'a str,
    ) -> BoxFuture<'a, Result<ActionState, ProviderError>> {
        async move {
            let a = self.client.action(parse_id(action_id)?).await?;
            Ok(match a.status.as_str() {
                "success" => ActionState::Done,
                "error" => ActionState::Failed(
                    a.error
                        .map(|e| format!("{} ({})", e.message, e.code))
                        .unwrap_or_else(|| "unknown error".into()),
                ),
                _ => ActionState::Running,
            })
        }
        .boxed()
    }

    fn ssh_keys(&self) -> BoxFuture<'_, Result<Vec<ProviderSshKey>, ProviderError>> {
        async move {
            Ok(self
                .client
                .ssh_keys()
                .await?
                .into_iter()
                .map(|k| ProviderSshKey {
                    id: k.id.to_string(),
                    name: k.name,
                    fingerprint: k.fingerprint,
                    public_key: Some(k.public_key),
                })
                .collect())
        }
        .boxed()
    }

    /// The control plane keeps the mesh up to date, not the CLI.
    fn sync_mesh<'a>(
        &'a self,
        _sources: &'a MeshSources,
    ) -> BoxFuture<'a, Result<(), ProviderError>> {
        async move { Ok(()) }.boxed()
    }

    /// Already open: the main firewall is attached when the server is made.
    fn open_node_port<'a>(
        &'a self,
        _id: &'a str,
        _name: &'a str,
        _port: u16,
    ) -> BoxFuture<'a, Result<(), ProviderError>> {
        async move { Ok(()) }.boxed()
    }
}

/// Secrets never appear in `Debug` output of the plan or errors.
impl std::fmt::Debug for CloudSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CloudSecret({})", self.provider().id())
    }
}

/// The token for `PUT .../integrations/<id>`.
pub fn integration_body(secret: &CloudSecret) -> (&'static str, &'static str, serde_json::Value) {
    match secret {
        CloudSecret::Hetzner(token) => (
            "Hetzner",
            "hetzner",
            serde_json::json!({ "token": token.expose_secret() }),
        ),
        CloudSecret::Aws(k) => (
            "AWS",
            "aws",
            serde_json::json!({
                "accessKeyId": k.access_key_id,
                "secretAccessKey": k.secret_access_key.expose_secret(),
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_matches_install_sh() {
        let ports: Vec<String> = firewall_rules()
            .into_iter()
            .map(|r| format!("{}/{}", r.protocol, r.port.as_deref().unwrap_or("-")))
            .collect();
        assert_eq!(
            ports,
            [
                "tcp/22",
                "tcp/80",
                "tcp/443",
                "udp/443",
                "tcp/20000-29999",
                "tcp/7101",
                "icmp/-"
            ]
        );
        assert!(firewall_rules().iter().all(|r| r.direction == "in"));
    }

    #[test]
    fn provider_wording() {
        assert_eq!(Provider::Aws.ssh_user(), "ubuntu");
        assert!(Provider::Aws.console().contains("Elastic IP"));
        assert!(Provider::Hetzner.approximate_prices());
        assert!(!Provider::Aws.approximate_prices());
    }
}
