//! Private dashboard (Cloudflare Tunnel + Access), the node's half. The
//! control plane sends an `AccessConfig` over the stream; modula-node (root)
//! applies it with systemd:
//!
//! - main server: `modula-cloudflared.service` runs `cloudflared tunnel run`
//!   with the tunnel token (from an env file, mode 0600); the edge stops
//!   serving the dashboard hostname; the node port is closed in ufw;
//! - other servers: `modula-cloudflared-access.service` runs
//!   `cloudflared access tcp` on 127.0.0.1:<node port> with a service token,
//!   and `node.json` points at it once a connection through it worked.
//!
//! cloudflared comes from Cloudflare's GitHub release as a .deb, pinned to
//! [`CLOUDFLARED_VERSION`] and checked against the SHA-256 sums below (taken
//! from the release notes). An already installed cloudflared is used as is.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use modula_proto::node::v1 as pb;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::edge::EdgeSink;
use crate::identity::Identity;

pub const CLOUDFLARED_VERSION: &str = "2026.10.3";
const CLOUDFLARED_SHA256: &[(&str, &str)] = &[
    (
        "amd64",
        "297d906a8fef396ec888b51571e783d594bde280f3b44d63e8fa343ef2c4cc4e",
    ),
    (
        "arm64",
        "9382c5406c5cb64821114360550b8903c600b20052e9e82359acf343834be2b7",
    ),
];
const CLOUDFLARED_PATHS: &[&str] = &["/usr/bin/cloudflared", "/usr/local/bin/cloudflared"];

pub const TUNNEL_UNIT: &str = "modula-cloudflared";
pub const ACCESS_UNIT: &str = "modula-cloudflared-access";
const CONF_DIR: &str = "/etc/modula";
const UNIT_DIR: &str = "/etc/systemd/system";
const STATE_FILE: &str = "access.json";
/// How long a new route through the tunnel may take to work.
const PROBE_FOR: Duration = Duration::from_secs(90);

/// What this node changed, kept in `<data dir>/access.json` so a restart
/// keeps hiding the dashboard and a later "off" knows what to undo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    #[serde(default)]
    pub hidden_routes: Vec<String>,
    /// The ufw rule this node deleted (re-added when private mode ends).
    #[serde(default)]
    pub closed_node_port: Option<u16>,
}

impl State {
    pub fn load(dir: &Path) -> Self {
        std::fs::read(dir.join(STATE_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        write_file(
            &dir.join(STATE_FILE),
            &serde_json::to_vec_pretty(self)?,
            0o644,
        )?;
        Ok(())
    }
}

#[async_trait]
pub trait Applier: Send + Sync {
    /// This node serves the dashboard (the main server).
    fn dashboard(&self) -> bool;
    async fn apply(&self, cfg: pb::AccessConfig) -> anyhow::Result<()>;
}

static APPLIER: OnceLock<Arc<dyn Applier>> = OnceLock::new();

/// Registered once by `modula-node run`; without it the node doesn't offer
/// private mode (tests, development).
pub fn register(a: Arc<dyn Applier>) {
    let _ = APPLIER.set(a);
}

pub fn applier() -> Option<Arc<dyn Applier>> {
    APPLIER.get().cloned()
}

/// Applies configs with systemd on this machine.
pub struct SystemdAccess {
    pub data_dir: PathBuf,
    pub edge: Option<Arc<dyn EdgeSink>>,
    pub dashboard: bool,
}

impl SystemdAccess {
    /// Restores the hidden dashboard route after a restart.
    pub fn new(data_dir: PathBuf, edge: Option<Arc<dyn EdgeSink>>, dashboard: bool) -> Self {
        if let Some(e) = &edge {
            e.set_hidden(State::load(&data_dir).hidden_routes);
        }
        Self {
            data_dir,
            edge,
            dashboard,
        }
    }

    async fn apply_main(&self, cfg: &pb::AccessConfig) -> anyhow::Result<()> {
        let mut st = State::load(&self.data_dir);
        if cfg.tunnel_token.is_empty() {
            remove_service(TUNNEL_UNIT, &env_path(TUNNEL_UNIT)).await?;
        } else {
            let bin = ensure_cloudflared(&self.data_dir).await?;
            let env = format!("TUNNEL_TOKEN={}\n", cfg.tunnel_token);
            install_service(TUNNEL_UNIT, &tunnel_unit(&bin), &env).await?;
        }
        st.hidden_routes = cfg.hidden_routes.clone();
        if let Some(e) = &self.edge {
            e.set_hidden(st.hidden_routes.clone());
        }
        let port = u16::try_from(cfg.node_port).unwrap_or(7101);
        match (cfg.close_node_port, st.closed_node_port) {
            (true, None) if ufw_has_rule(port).await => {
                run("ufw", &["delete", "allow", &format!("{port}/tcp")]).await?;
                st.closed_node_port = Some(port);
            }
            (false, Some(p)) => {
                run("ufw", &["allow", &format!("{p}/tcp")]).await?;
                st.closed_node_port = None;
            }
            _ => {}
        }
        st.save(&self.data_dir)
    }

    async fn apply_worker(&self, cfg: &pb::AccessConfig) -> anyhow::Result<()> {
        let mut id = Identity::load(&self.data_dir)?;
        if cfg.node_host.is_empty() {
            // Back to dialing the control plane directly.
            let direct = cfg.direct_server.clone();
            if !direct.starts_with("https://") {
                bail!("no direct server address to go back to");
            }
            if id.meta.server != direct {
                probe(&direct, &id).await?;
                id.meta.server = direct;
                id.save()?;
            }
            // Later, so this answer still travels through the tunnel.
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if let Err(e) = remove_service(ACCESS_UNIT, &env_path(ACCESS_UNIT)).await {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "could not remove the access proxy"
                    );
                }
            });
            return Ok(());
        }
        let port = local_port(&cfg.tunneled_server)?;
        setup_access_proxy(
            &self.data_dir,
            &cfg.node_host,
            port,
            &cfg.client_id,
            &cfg.client_secret,
        )
        .await?;
        if id.meta.server != cfg.tunneled_server {
            probe(&cfg.tunneled_server, &id).await?;
            id.meta.server = cfg.tunneled_server.clone();
            id.save()?;
        }
        Ok(())
    }
}

#[async_trait]
impl Applier for SystemdAccess {
    fn dashboard(&self) -> bool {
        self.dashboard
    }

    async fn apply(&self, cfg: pb::AccessConfig) -> anyhow::Result<()> {
        if cfg.main {
            if !self.dashboard {
                bail!("this server doesn't serve the dashboard");
            }
            self.apply_main(&cfg).await
        } else {
            self.apply_worker(&cfg).await
        }
    }
}

/// Installs and starts the local `cloudflared access tcp` listener; used when
/// moving to the tunnel and by `modula-node join --access-host …`.
pub async fn setup_access_proxy(
    data_dir: &Path,
    host: &str,
    port: u16,
    client_id: &str,
    client_secret: &str,
) -> anyhow::Result<()> {
    if !valid_host(host) {
        bail!("{host:?} is not a hostname");
    }
    let bin = ensure_cloudflared(data_dir).await?;
    let env = access_env(client_id, client_secret)?;
    install_service(ACCESS_UNIT, &access_unit(&bin, host, port), &env).await?;
    // Wait for the listener.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("cloudflared did not start listening on 127.0.0.1:{port} (journalctl -u {ACCESS_UNIT})")
}

/// Connects (mTLS, pinned root) through `server` until it works.
async fn probe(server: &str, id: &Identity) -> anyhow::Result<()> {
    let pin = id.fingerprint()?;
    let started = std::time::Instant::now();
    loop {
        match crate::join::connect(server, pin, Some(id)).await {
            Ok(_) => return Ok(()),
            Err(e) if started.elapsed() > PROBE_FOR => {
                return Err(e.context(format!("cannot reach the control plane through {server}")));
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

/// `https://127.0.0.1:7101` → 7101.
pub fn local_port(server: &str) -> anyhow::Result<u16> {
    let rest = server
        .strip_prefix("https://127.0.0.1:")
        .context("the tunneled server address must be https://127.0.0.1:<port>")?;
    rest.trim_end_matches('/')
        .parse()
        .context("invalid port in the tunneled server address")
}

fn valid_host(h: &str) -> bool {
    crate::edge::normalize_host(h).is_some_and(|n| n == h && n.contains('.'))
}

fn env_path(unit: &str) -> PathBuf {
    Path::new(CONF_DIR).join(format!("{unit}.env"))
}

pub fn tunnel_unit(bin: &Path) -> String {
    format!(
        "[Unit]
Description=Modula Deploy private dashboard (Cloudflare Tunnel)
After=network-online.target
Wants=network-online.target

[Service]
# TUNNEL_TOKEN; written by modula-node, mode 0600.
EnvironmentFile={env}
ExecStart={bin} --no-autoupdate tunnel run
Restart=always
RestartSec=2
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
",
        env = env_path(TUNNEL_UNIT).display(),
        bin = bin.display()
    )
}

pub fn access_unit(bin: &Path, host: &str, port: u16) -> String {
    format!(
        "[Unit]
Description=Modula Deploy node connection through Cloudflare Access
After=network-online.target
Wants=network-online.target
Before=modula-node.service

[Service]
# TUNNEL_SERVICE_TOKEN_ID and _SECRET; written by modula-node, mode 0600.
EnvironmentFile={env}
ExecStart={bin} access tcp --hostname {host} --url 127.0.0.1:{port}
Restart=always
RestartSec=2
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
",
        env = env_path(ACCESS_UNIT).display(),
        bin = bin.display()
    )
}

/// The service token as cloudflared reads it from the environment.
pub fn access_env(client_id: &str, client_secret: &str) -> anyhow::Result<String> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
    };
    if !ok(client_id) || !ok(client_secret) {
        bail!("the service token has unexpected characters");
    }
    Ok(format!(
        "TUNNEL_SERVICE_TOKEN_ID={client_id}\nTUNNEL_SERVICE_TOKEN_SECRET={client_secret}\n"
    ))
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<bool> {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::identity::write_atomic(path, bytes, mode)?;
    Ok(true)
}

/// Run `bin` with `args` (argv only); its stdout, or an error with stderr.
pub(crate) async fn run(bin: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .with_context(|| format!("cannot run {bin}"))?;
    if !out.status.success() {
        bail!(
            "{bin} {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn install_service(unit: &str, contents: &str, env: &str) -> anyhow::Result<()> {
    let env_changed = write_file(&env_path(unit), env.as_bytes(), 0o600)?;
    let unit_path = Path::new(UNIT_DIR).join(format!("{unit}.service"));
    let unit_changed = write_file(&unit_path, contents.as_bytes(), 0o644)?;
    if unit_changed {
        run("systemctl", &["daemon-reload"]).await?;
    }
    run("systemctl", &["enable", unit]).await?;
    let verb = if env_changed || unit_changed {
        "restart"
    } else {
        "start"
    };
    run("systemctl", &[verb, unit]).await?;
    Ok(())
}

async fn remove_service(unit: &str, env: &Path) -> anyhow::Result<()> {
    let unit_path = Path::new(UNIT_DIR).join(format!("{unit}.service"));
    if !unit_path.exists() {
        let _ = std::fs::remove_file(env);
        return Ok(());
    }
    run("systemctl", &["disable", "--now", unit]).await?;
    std::fs::remove_file(&unit_path)?;
    let _ = std::fs::remove_file(env);
    run("systemctl", &["daemon-reload"]).await?;
    Ok(())
}

async fn ufw_has_rule(port: u16) -> bool {
    let Ok(out) = run("ufw", &["status"]).await else {
        return false;
    };
    out.starts_with("Status: active")
        && out
            .lines()
            .any(|l| l.split_whitespace().next() == Some(&format!("{port}/tcp")))
}

/// The comment on the ufw rules modula adds (install.sh uses it too).
const UFW_TAG: &str = "modula";

/// ufw is installed and active (and we may run it).
pub(crate) async fn ufw_active() -> bool {
    run("ufw", &["status"])
        .await
        .is_ok_and(|out| out.starts_with("Status: active"))
}

/// `ufw allow <port>/tcp comment modula`.
pub(crate) async fn ufw_open(port: u16) -> anyhow::Result<()> {
    let spec = format!("{port}/tcp");
    run("ufw", &["allow", &spec, "comment", UFW_TAG]).await?;
    Ok(())
}

/// Deletes the rule [`ufw_open`] added (matching may depend on the comment
/// in some ufw versions, so both forms are tried).
pub(crate) async fn ufw_close(port: u16) -> anyhow::Result<()> {
    let spec = format!("{port}/tcp");
    if run("ufw", &["delete", "allow", &spec, "comment", UFW_TAG])
        .await
        .is_ok()
    {
        return Ok(());
    }
    run("ufw", &["delete", "allow", &spec]).await?;
    Ok(())
}

/// TCP ports in `range` with a modula-tagged rule in `ufw status` output.
pub(crate) fn ufw_tagged_ports(status: &str, range: std::ops::RangeInclusive<u16>) -> Vec<u16> {
    let mut ports: Vec<u16> = status
        .lines()
        .filter(|l| l.trim_end().ends_with(&format!("# {UFW_TAG}")))
        .filter_map(|l| {
            l.split_whitespace()
                .next()?
                .strip_suffix("/tcp")?
                .parse()
                .ok()
        })
        .filter(|p| range.contains(p))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// The modula-tagged TCP ports in `range` that ufw allows now.
pub(crate) async fn ufw_open_ports(range: std::ops::RangeInclusive<u16>) -> Vec<u16> {
    match run("ufw", &["status"]).await {
        Ok(out) if out.starts_with("Status: active") => ufw_tagged_ports(&out, range),
        _ => vec![],
    }
}

fn deb_arch() -> anyhow::Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        other => bail!("no cloudflared package for {other}"),
    }
}

pub fn expected_sha256(arch: &str) -> Option<&'static str> {
    CLOUDFLARED_SHA256
        .iter()
        .find(|(a, _)| *a == arch)
        .map(|(_, h)| *h)
}

/// The cloudflared binary, installed from the pinned release if missing.
pub async fn ensure_cloudflared(data_dir: &Path) -> anyhow::Result<PathBuf> {
    let found = || {
        CLOUDFLARED_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
    };
    if let Some(p) = found() {
        return Ok(p);
    }
    let arch = deb_arch()?;
    let want = expected_sha256(arch).context("no pinned checksum")?;
    let url = format!(
        "https://github.com/cloudflare/cloudflared/releases/download/{CLOUDFLARED_VERSION}/cloudflared-linux-{arch}.deb"
    );
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder()
        .user_agent(concat!("modula-node/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()?;
    let bytes = http
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("cannot download {url}"))?
        .bytes()
        .await?;
    let got = hex::encode(Sha256::digest(&bytes));
    if got != want {
        bail!(
            "cloudflared package checksum mismatch (expected {want}, got {got}); refusing to install"
        );
    }
    let deb = data_dir.join(format!("cloudflared-{CLOUDFLARED_VERSION}.deb"));
    write_file(&deb, &bytes, 0o600)?;
    let installed = run("dpkg", &["-i", &deb.to_string_lossy()]).await;
    let _ = std::fs::remove_file(&deb);
    installed.context("installing cloudflared")?;
    found().context("cloudflared was installed but its binary is not where expected")
}

/// `modula-node run` answers `AccessConfig` with this.
pub async fn handle(cfg: pb::AccessConfig) -> pb::AccessStatus {
    let request_id = cfg.request_id.clone();
    let result = match applier() {
        Some(a) => a.apply(cfg).await,
        None => Err(anyhow::anyhow!("this server can't run cloudflared")),
    };
    match result {
        Ok(()) => pb::AccessStatus {
            request_id,
            ok: true,
            error: String::new(),
        },
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "private dashboard change failed");
            pb::AccessStatus {
                request_id,
                ok: false,
                error: format!("{e:#}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_keep_secrets_in_env_files() {
        let t = tunnel_unit(Path::new("/usr/bin/cloudflared"));
        assert!(t.contains("ExecStart=/usr/bin/cloudflared --no-autoupdate tunnel run\n"));
        assert!(t.contains("EnvironmentFile=/etc/modula/modula-cloudflared.env"));
        let a = access_unit(
            Path::new("/usr/bin/cloudflared"),
            "deploy-nodes.example.com",
            7101,
        );
        assert!(a.contains(
            "ExecStart=/usr/bin/cloudflared access tcp --hostname deploy-nodes.example.com --url 127.0.0.1:7101\n"
        ));
        assert!(!a.contains("secret"));
        assert_eq!(
            access_env("abc.access", "s3cr3t").unwrap(),
            "TUNNEL_SERVICE_TOKEN_ID=abc.access\nTUNNEL_SERVICE_TOKEN_SECRET=s3cr3t\n"
        );
        assert!(access_env("a\nX=1", "s").is_err());
    }

    #[test]
    fn ports_and_hosts() {
        assert_eq!(local_port("https://127.0.0.1:7101").unwrap(), 7101);
        assert!(local_port("https://example.com:7101").is_err());
        assert!(valid_host("a-nodes.example.com"));
        assert!(!valid_host("A.example.com"));
        assert!(!valid_host("x --url y"));
    }

    #[test]
    fn tagged_ufw_ports() {
        let status = "Status: active\n\nTo                         Action      From\n\
--                         ------      ----\n\
22/tcp                     ALLOW       Anywhere                   # modula\n\
20001/tcp                  ALLOW       Anywhere                   # modula\n\
20002/tcp                  ALLOW       Anywhere\n\
20000:29999/tcp            ALLOW       Anywhere                   # modula\n\
20001/tcp (v6)             ALLOW       Anywhere (v6)              # modula\n";
        assert_eq!(ufw_tagged_ports(status, 20000..=29999), [20001]);
    }

    #[test]
    fn checksums_are_pinned_per_arch() {
        for arch in ["amd64", "arm64"] {
            let h = expected_sha256(arch).unwrap();
            assert_eq!(h.len(), 64);
            assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn state_round_trips() {
        let dir = std::env::temp_dir().join(format!("modula-access-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(State::load(&dir), State::default());
        let s = State {
            hidden_routes: vec!["deploy.example.com".into()],
            closed_node_port: Some(7101),
        };
        s.save(&dir).unwrap();
        assert_eq!(State::load(&dir), s);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
