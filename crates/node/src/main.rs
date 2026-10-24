//! `modula-node` command line.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use modula_node::build::buildkit::{BuildkitBuilder, Limits};
use modula_node::build::{BuildLogs, BuildSupport};
use modula_node::edge::EdgeSink;
use modula_node::edge::ask::{self, Allowlist};
use modula_node::edge::caddy::{CaddyEdge, Listeners};
use modula_node::identity::Identity;
use modula_node::join::{JoinArgs, join};
use modula_node::reconcile::Capabilities;
use modula_node::runtime::docker::DockerRuntime;
use modula_node::session;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "modula-node",
    version,
    about = "Runs Modula Deploy workloads on this server"
)]
struct Cli {
    /// Where the node identity is kept.
    #[arg(
        long,
        global = true,
        env = "MODULA_NODE_DIR",
        default_value = "/var/lib/modula-node"
    )]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Join a control plane using a one-time token from the dashboard.
    Join {
        /// Control-plane node endpoint, e.g. https://deploy.example.com:7101
        #[arg(long)]
        server: String,
        /// Single-use join token (or set MODULA_JOIN_TOKEN).
        #[arg(long, env = "MODULA_JOIN_TOKEN", hide_env_values = true)]
        token: String,
        /// Pinned root certificate fingerprint (`sha256:…`).
        #[arg(long)]
        ca: String,
        /// Display name for this server (defaults to the hostname).
        #[arg(long)]
        name: Option<String>,
        /// Private mode: reach the node endpoint through Cloudflare Access at
        /// this hostname (a local `cloudflared access tcp` on --server's port).
        #[arg(long, requires_all = ["access_id", "access_secret"])]
        access_host: Option<String>,
        /// The Access service token's client id.
        #[arg(long)]
        access_id: Option<String>,
        /// The Access service token's client secret (or MODULA_ACCESS_SECRET).
        #[arg(long, env = "MODULA_ACCESS_SECRET", hide_env_values = true)]
        access_secret: Option<String>,
    },
    /// Connect to the control plane and run workloads.
    Run {
        /// Caddy admin API: `unix:/path` or `host:port`.
        #[arg(
            long,
            env = "MODULA_CADDY_ADMIN",
            default_value = "unix:/run/caddy/admin.sock"
        )]
        caddy_admin: String,
        /// Loopback address for Caddy's on-demand TLS `ask` endpoint.
        #[arg(long, env = "MODULA_ASK_LISTEN", default_value = "127.0.0.1:7102")]
        ask_listen: std::net::SocketAddr,
        /// Run workloads without managing Caddy (no public routes).
        #[arg(long)]
        no_edge: bool,
        /// A route this server serves itself, as HOST=ADDR:PORT (repeatable).
        /// The installer uses it for the dashboard on the main server.
        #[arg(long = "route", value_name = "HOST=ADDR", value_parser = modula_node::edge::parse_local_route)]
        routes: Vec<modula_node::edge::ResolvedRoute>,
        /// Don't build images from source on this server.
        #[arg(long)]
        no_build: bool,
        /// BuildKit client binary.
        #[arg(long, env = "MODULA_BUILDCTL", default_value = "buildctl")]
        buildctl: PathBuf,
        /// Railpack binary.
        #[arg(long, env = "MODULA_RAILPACK", default_value = "railpack")]
        railpack: PathBuf,
        /// Directory for BuildKit's sockets (one subdirectory per workspace,
        /// shared with that workspace's build container).
        #[arg(long, default_value = "/run/modula-buildkit")]
        buildkit_dir: PathBuf,
        /// Memory limit of each workspace's build engine, in MiB.
        #[arg(long, env = "MODULA_BUILD_MEMORY_MB", default_value_t = 2048)]
        build_memory_mb: u32,
        /// CPU limit of each workspace's build engine, in cores.
        #[arg(long, env = "MODULA_BUILD_CPUS", default_value_t = 2.0)]
        build_cpus: f64,
        /// How many builds run on this server at once.
        #[arg(long, env = "MODULA_BUILD_CONCURRENCY", default_value_t = 2)]
        build_concurrency: usize,
        /// Caddy runs socket-activated (modula-caddy.socket passes TCP 80,
        /// TCP 443 and UDP 443 as fds 3–5), so its ports stay open across restarts.
        #[arg(long, env = "MODULA_CADDY_SOCKETS")]
        caddy_sockets: bool,
        /// Where upgrade requests for the modula-upgrade unit go. Upgrades on
        /// the control plane's request are offered only when it exists.
        #[arg(long, default_value = "/var/lib/modula-upgrade")]
        upgrade_dir: PathBuf,
        /// Stay out of the private network between servers (WireGuard).
        #[arg(long, env = "MODULA_NO_MESH")]
        no_mesh: bool,
        /// UDP port of this server's WireGuard tunnel to the others.
        #[arg(long, env = "MODULA_WG_PORT", default_value_t = modula_node::mesh::DEFAULT_PORT)]
        wg_port: u16,
        /// Where backups to this server's disk are kept.
        #[arg(long, env = "MODULA_BACKUP_DIR", default_value = modula_node::backup::DEFAULT_DIR)]
        backup_dir: PathBuf,
        /// WAL-G binary, mounted into Postgres containers with point-in-time
        /// recovery.
        #[arg(long = "wal-g", env = "MODULA_WALG", default_value = "wal-g")]
        walg: PathBuf,
    },
    /// Show this server's identity.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "modula_node=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Join {
            server,
            token,
            ca,
            name,
            access_host,
            access_id,
            access_secret,
        } => {
            if let (Some(host), Some(id), Some(secret)) = (&access_host, &access_id, &access_secret)
            {
                let port = modula_node::access::local_port(&server)?;
                std::fs::create_dir_all(&cli.data_dir)?;
                modula_node::access::setup_access_proxy(&cli.data_dir, host, port, id, secret)
                    .await?;
            }
            let hostname = name.unwrap_or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_default()
            });
            let id = join(JoinArgs {
                server: &server,
                token: &token,
                fingerprint: &ca,
                hostname,
                dir: &cli.data_dir,
            })
            .await?;
            println!(
                "Joined as node {}. Start it with: modula-node run",
                id.meta.node_id
            );
        }
        Command::Run {
            caddy_admin,
            ask_listen,
            no_edge,
            routes,
            no_build,
            buildctl,
            railpack,
            buildkit_dir,
            build_memory_mb,
            build_cpus,
            build_concurrency,
            caddy_sockets,
            upgrade_dir,
            no_mesh,
            wg_port,
            backup_dir,
            walg,
        } => {
            modula_node::backup::set_dir(backup_dir);
            modula_node::backup::walg::set_bin(walg);
            let runtime = Arc::new(DockerRuntime::connect()?);
            // Containers must not reach the cloud's metadata service.
            tokio::spawn(modula_node::runtime::firewall::block_metadata());
            // Only the main server serves routes of its own (the dashboard).
            let serves_dashboard = !routes.is_empty();
            let stop = CancellationToken::new();
            let edge: Option<Arc<dyn EdgeSink>> = if no_edge {
                None
            } else {
                anyhow::ensure!(
                    ask_listen.ip().is_loopback(),
                    "--ask-listen must be a loopback address"
                );
                let allowlist = Allowlist::default();
                let listener = tokio::net::TcpListener::bind(ask_listen)
                    .await
                    .with_context(|| format!("cannot bind {ask_listen}"))?;
                tokio::spawn(ask::serve(listener, allowlist.clone(), stop.clone()));
                let caddy = Arc::new(CaddyEdge::new(
                    &caddy_admin,
                    format!("http://{ask_listen}/ask"),
                    allowlist,
                    routes,
                    if caddy_sockets {
                        Listeners::SystemdSockets
                    } else {
                        Listeners::Ports
                    },
                )?);
                tokio::spawn(caddy.clone().watch(stop.clone()));
                Some(caddy)
            };
            tokio::spawn({
                let stop = stop.clone();
                async move {
                    shutdown_signal().await;
                    stop.cancel();
                }
            });
            let build = if no_build {
                None
            } else {
                let logs = BuildLogs::new(cli.data_dir.join("build-logs"))?;
                anyhow::ensure!(
                    build_memory_mb >= 256 && build_cpus > 0.0 && build_concurrency > 0,
                    "build limits must be at least 256 MiB, some CPU and one build"
                );
                let limits = Limits {
                    memory_bytes: i64::from(build_memory_mb) << 20,
                    nano_cpus: (build_cpus * 1e9) as i64,
                    concurrency: build_concurrency,
                    ..Limits::default()
                };
                let builder =
                    BuildkitBuilder::new(&cli.data_dir, buildkit_dir, buildctl, railpack, limits)?;
                Some(BuildSupport {
                    builder: Arc::new(builder),
                    logs,
                })
            };
            let db: Arc<dyn modula_node::db::Executor> =
                Arc::new(modula_node::db::pg::Postgres::new(&cli.data_dir));
            let upgrade = modula_node::upgrade::SystemdUpgrader::detect(&upgrade_dir)
                .map(|u| Arc::new(u) as Arc<dyn modula_node::upgrade::Upgrader>);
            modula_node::access::register(Arc::new(modula_node::access::SystemdAccess::new(
                cli.data_dir.clone(),
                edge.clone(),
                serves_dashboard,
            )));
            let mesh = if no_mesh {
                None
            } else {
                open_mesh(&cli.data_dir, wg_port).await
            };
            let caps = Capabilities {
                edge,
                build,
                db: Some(db),
                upgrade,
                mesh,
            };
            session::run(cli.data_dir, runtime, caps, stop).await?;
        }
        Command::Status => {
            let id = Identity::load(&cli.data_dir)?;
            let expires = id.cert_not_after().context("cannot read the certificate")?;
            let days = expires
                .duration_since(std::time::SystemTime::now())
                .map_or(0, |d| d.as_secs() / 86_400);
            println!("node      {}", id.meta.node_id);
            println!("server    {}", id.meta.server);
            println!("ca        {}", id.meta.fingerprint);
            println!("cert      expires in {days} days");
        }
    }
    Ok(())
}

/// The node's side of the private network, or None (warned) when this host
/// can't take part: not Linux, or no WireGuard.
async fn open_mesh(dir: &std::path::Path, port: u16) -> Option<Arc<modula_node::mesh::Mesh>> {
    #[cfg(target_os = "linux")]
    {
        match modula_node::mesh::linux::LinuxMesh::open(dir) {
            Ok((sys, public_key)) => {
                modula_node::mesh::linux::open_ufw_port(port).await;
                Some(modula_node::mesh::Mesh::new(
                    Arc::new(sys),
                    public_key,
                    port,
                ))
            }
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "private networking is off");
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (dir, port);
        tracing::info!("private networking needs Linux; this server stays out of it");
        None
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
