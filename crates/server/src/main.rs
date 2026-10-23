use std::path::PathBuf;

use clap::{Parser, Subcommand};
use modula_server::config::{self, Config};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "modula-server", version, about = "Modula Deploy control plane")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the API server (default).
    Serve,
    /// Apply database migrations and exit.
    Migrate,
    /// Create a new master key file (mode 0600). Back it up: without it, secrets are unrecoverable.
    Keygen {
        #[arg(long, env = "MODULA_MASTER_KEY_FILE")]
        out: PathBuf,
    },
    /// Replace the first-run setup token and print it (only while setup is pending).
    SetupToken,
    /// Print a single-use command that joins a new server to this instance.
    JoinToken {
        /// Minutes until the token expires.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u16).range(1..=1440))]
        ttl_minutes: u16,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MODULA_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")),
        )
        .init();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => modula_server::serve(Config::from_env()?).await,
        Command::Migrate => {
            let cfg = Config::from_env()?;
            let db = modula_server::db::connect(&cfg.database_url).await?;
            modula_server::db::migrate(&db).await
        }
        Command::Keygen { out } => {
            let key = config::write_new_master_key(&out)?;
            eprintln!("wrote master key {} to {}", key.id(), out.display());
            Ok(())
        }
        Command::JoinToken { ttl_minutes } => {
            use secrecy::ExposeSecret;
            let cfg = Config::from_env()?;
            let db = modula_server::db::connect(&cfg.database_url).await?;
            modula_server::db::migrate(&db).await?;
            let keys = modula_server::keyring::Keyring::new(cfg.load_master_key()?);
            let pki = modula_server::pki::Pki::load_or_init(&db, &keys).await?;
            let ttl = time::Duration::minutes(i64::from(ttl_minutes));
            let (token, expires) = modula_server::nodes::create_join_token(&db, None, ttl)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            eprintln!("Run this on the new server (expires {expires}):");
            println!(
                "{}",
                modula_server::nodes::join_command(&cfg, token.expose_secret(), &pki.fingerprint())
            );
            Ok(())
        }
        Command::SetupToken => {
            use secrecy::ExposeSecret;
            let cfg = Config::from_env()?;
            let db = modula_server::db::connect(&cfg.database_url).await?;
            match modula_server::auth::setup::issue_if_pending(&db).await? {
                Some(t) => println!("{}", t.expose_secret()),
                None => eprintln!("setup already completed"),
            }
            Ok(())
        }
    }
}
