//! `modula-node` command line.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use modula_node::edge::EdgeSink;
use modula_node::edge::ask::{self, Allowlist};
use modula_node::edge::caddy::CaddyEdge;
use modula_node::identity::Identity;
use modula_node::join::{JoinArgs, join};
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
        } => {
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
        } => {
            let runtime = Arc::new(DockerRuntime::connect()?);
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
                Some(Arc::new(CaddyEdge::new(
                    &caddy_admin,
                    format!("http://{ask_listen}/ask"),
                    allowlist,
                )?))
            };
            tokio::spawn({
                let stop = stop.clone();
                async move {
                    shutdown_signal().await;
                    stop.cancel();
                }
            });
            session::run(cli.data_dir, runtime, edge, stop).await?;
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
