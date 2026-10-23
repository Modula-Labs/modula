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
    /// Restore the control plane's database from one of its own backups
    /// (`…/modula-control-plane/….dump.age`, downloaded from the bucket).
    /// Stop the server first; see backup::control_plane for the procedure.
    RestoreBackup {
        /// The encrypted backup file.
        file: PathBuf,
        /// The master key the backup was made under.
        #[arg(long, env = "MODULA_MASTER_KEY_FILE")]
        master_key_file: PathBuf,
        #[arg(long, env = "MODULA_DATABASE_URL", hide_env_values = true)]
        database_url: Option<String>,
        /// Write the decrypted pg_dump archive to stdout instead of restoring.
        #[arg(long)]
        decrypt_only: bool,
    },
    /// List what this build supports (the installer checks before using it).
    Features,
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
        Command::RestoreBackup {
            file,
            master_key_file,
            database_url,
            decrypt_only,
        } => {
            let master = config::load_master_key(&master_key_file)?;
            modula_server::backup::control_plane::restore_cli(
                &file,
                &master,
                database_url.as_deref(),
                decrypt_only,
            )?;
            if !decrypt_only {
                eprintln!("restored; start modula-server to apply any newer migrations");
            }
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
        Command::Features => {
            for f in modula_server::FEATURES {
                println!("{f}");
            }
            Ok(())
        }
        Command::SetupToken => {
            use secrecy::ExposeSecret;
            let cfg = Config::from_env()?;
            let db = modula_server::db::connect(&cfg.database_url).await?;
            match modula_server::auth::setup::issue_if_pending(&db).await? {
                Some(t) => {
                    if modula_server::auth::setup::preseeded_from_env()?.is_some() {
                        eprintln!(
                            "note: this replaces the preseeded setup token until modula-server restarts \
                             (remove {} from its environment to keep this one)",
                            modula_server::auth::setup::PRESEEDED_HASH_ENV
                        );
                    }
                    println!("{}", t.expose_secret())
                }
                None => eprintln!("setup already completed"),
            }
            Ok(())
        }
    }
}
