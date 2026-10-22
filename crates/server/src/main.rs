use std::path::PathBuf;

use clap::{Parser, Subcommand};
use modula_server::config::{self, Config};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "modula-server", version, about = "Modula Cloud control plane")]
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
