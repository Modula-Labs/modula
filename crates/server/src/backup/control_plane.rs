//! Backups of the control plane's own database: `pg_dump` run by the server,
//! age-encrypted as it streams and uploaded to the target an instance admin
//! chose (Settings → Backups, `instance_backup`).
//!
//! The age identity is derived from the master key, not stored: these
//! backups exist for when the database is gone, so decrypting them needs
//! only the master key file (which must be backed up separately anyway).
//!
//! Restoring, on a fresh install of the same version:
//!
//! 1. Stop `modula-server` and download the latest
//!    `<prefix>modula-control-plane/<time>-<id>.dump.age` from the bucket.
//! 2. Put the old master key file in place (`MODULA_MASTER_KEY_FILE`).
//! 3. Run `modula-server restore-backup <file>`: it decrypts the dump and
//!    pipes it into `pg_restore --clean --if-exists --single-transaction
//!    --no-owner` against `MODULA_DATABASE_URL`. (`--decrypt-only` writes
//!    the pg_dump archive to stdout instead.)
//! 4. Start `modula-server`; nodes reconnect with their existing certificates.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, bail};
use modula_backup::keys::Identity;
use modula_crypto::envelope::MasterKey;
use modula_id::Id;
use tokio::io::AsyncReadExt;
use url::Url;
use zeroize::Zeroizing;

use super::{Kind, PART_URLS, Store, load_target, part_size, run};
use crate::keyring::Keyring;
use crate::state::AppState;

const KEY_PURPOSE: &str = "backup/control-plane/age-identity";

pub fn identity(keys: &Keyring) -> anyhow::Result<Identity> {
    modula_backup::keys::identity_from_seed(&keys.derive_from_master(KEY_PURPOSE))
}

pub fn identity_from_master(master: &MasterKey) -> anyhow::Result<Identity> {
    modula_backup::keys::identity_from_seed(&master.derive(KEY_PURPOSE))
}

/// Start the daily self-backup if it is due.
pub async fn start_due(state: &AppState) -> anyhow::Result<()> {
    let due: Option<Id> = sqlx::query_scalar(concat!(
        "UPDATE instance_backup SET last_scheduled_at = now()
         WHERE enabled AND target_id IS NOT NULL AND ",
        super::schedule::due!(),
        " RETURNING target_id"
    ))
    .fetch_optional(&state.db)
    .await?;
    if let Some(target) = due
        && let Err(e) = run::start(state, None, target, Kind::Scheduled, None).await
    {
        tracing::warn!(error = ?e, "could not start the control plane backup");
    }
    Ok(())
}

/// Run one self-backup to completion and record the outcome.
pub async fn run(state: AppState, backup: Id) {
    match dump_and_upload(&state, backup).await {
        Ok(size) => {
            let done = sqlx::query(
                "UPDATE backups SET status = 'completed', size_bytes = $2, upload_id = NULL,
                        finished_at = now()
                 WHERE id = $1 AND status = 'running'",
            )
            .bind(backup)
            .bind(i64::try_from(size).unwrap_or(i64::MAX))
            .execute(&state.db)
            .await;
            match done {
                Ok(_) => tracing::info!(%backup, bytes = size, "control plane backup completed"),
                Err(e) => tracing::error!(%backup, error = %e, "cannot record a backup"),
            }
        }
        Err(e) => {
            if let Err(e) = run::fail(&state, backup, &format!("{e:#}")).await {
                tracing::error!(%backup, error = %e, "cannot record a failed backup");
            }
        }
    }
}

async fn dump_and_upload(state: &AppState, backup: Id) -> anyhow::Result<u64> {
    let (target_id, key): (Id, String) =
        sqlx::query_as("SELECT target_id, object_key FROM backups WHERE id = $1")
            .bind(backup)
            .fetch_one(&state.db)
            .await?;
    let Store::Remote(target) = load_target(state, target_id).await.map_err(run::app)?.1 else {
        bail!("the control plane backs up only to S3-compatible storage it reaches");
    };
    let http = super::s3::http()?;
    let previous: Option<i64> = sqlx::query_scalar(
        "SELECT size_bytes FROM backups WHERE service_instance_id IS NULL AND status = 'completed'
         ORDER BY started_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await?
    .flatten();
    let part_size = part_size(previous);
    let recipient = identity(&state.keys)?.to_public();

    let upload_id = target.create_multipart(http, &key).await?;
    sqlx::query("UPDATE backups SET upload_id = $2 WHERE id = $1")
        .bind(backup)
        .bind(&upload_id)
        .execute(&state.db)
        .await?;
    let urls = target.presign_parts(&key, &upload_id, PART_URLS)?;

    let mut child = pg_dump(&state.cfg.database_url)?
        .spawn()
        .context("cannot run pg_dump (is the PostgreSQL client installed?)")?;
    let stdout = child.stdout.take().context("pg_dump has no stdout")?;
    let mut stderr = child.stderr.take().context("pg_dump has no stderr")?;
    let stderr = tokio::spawn(async move {
        let mut text = String::new();
        let _ = (&mut stderr)
            .take(64 * 1024)
            .read_to_string(&mut text)
            .await;
        text
    });
    let uploaded = modula_backup::stream::encrypt_and_upload(
        http,
        tokio_util::io::ReaderStream::new(stdout),
        &recipient,
        &urls,
        usize::try_from(part_size)?,
    )
    .await;
    // If the upload failed first, pg_dump has lost its reader and exits too.
    let status = child.wait().await?;
    let stderr = stderr.await.unwrap_or_default();
    if !status.success() {
        bail!("pg_dump failed ({status}): {}", stderr.trim());
    }
    let uploaded = uploaded?;
    target
        .complete_multipart(http, &key, &upload_id, &uploaded.etags)
        .await?;
    Ok(uploaded.size_bytes)
}

/// The database URL without its password, which goes in `PGPASSWORD`
/// instead of argv (visible to every local user).
fn split_password(database_url: &str) -> anyhow::Result<(String, Option<Zeroizing<String>>)> {
    let mut url: Url = database_url.parse().context("invalid database URL")?;
    let password = url.password().map(|p| {
        Zeroizing::new(
            percent_encoding::percent_decode_str(p)
                .decode_utf8_lossy()
                .into_owned(),
        )
    });
    url.set_password(None)
        .map_err(|()| anyhow::anyhow!("invalid database URL"))?;
    Ok((url.into(), password))
}

fn pg_dump(database_url: &str) -> anyhow::Result<tokio::process::Command> {
    let (url, password) = split_password(database_url)?;
    let mut cmd = tokio::process::Command::new("pg_dump");
    cmd.arg("--format=custom")
        .arg(format!("--dbname={url}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(p) = password {
        cmd.env("PGPASSWORD", p.as_str());
    }
    Ok(cmd)
}

/// `modula-server restore-backup`: decrypt a self-backup and restore it into
/// `database_url` (or write the pg_dump archive to stdout).
pub fn restore_cli(
    file: &Path,
    master: &MasterKey,
    database_url: Option<&str>,
    decrypt_only: bool,
) -> anyhow::Result<()> {
    let identity = identity_from_master(master)?;
    let input =
        std::fs::File::open(file).with_context(|| format!("cannot open {}", file.display()))?;
    if decrypt_only {
        let mut plaintext = decrypt(input, &identity)?;
        std::io::copy(&mut plaintext, &mut std::io::stdout().lock())?;
        return Ok(());
    }
    restore(
        input,
        &identity,
        database_url.context("set MODULA_DATABASE_URL (or pass --database-url)")?,
    )
}

fn decrypt(input: impl std::io::Read, identity: &Identity) -> anyhow::Result<impl std::io::Read> {
    modula_backup::stream::decrypt_reader(input, identity)
        .context("cannot decrypt: is this a control plane backup made with this master key?")
}

/// Decrypt a self-backup and pipe it into `pg_restore`. Objects end up owned
/// by the restoring user, which need not be the one that made the dump.
pub fn restore(
    input: impl std::io::Read,
    identity: &Identity,
    database_url: &str,
) -> anyhow::Result<()> {
    let mut plaintext = decrypt(input, identity)?;
    let (url, password) = split_password(database_url)?;
    let mut cmd = std::process::Command::new("pg_restore");
    cmd.args([
        "--clean",
        "--if-exists",
        "--single-transaction",
        "--no-owner",
    ])
    .arg(format!("--dbname={url}"))
    .stdin(Stdio::piped());
    if let Some(p) = &password {
        cmd.env("PGPASSWORD", p.as_str());
    }
    let mut child = cmd
        .spawn()
        .context("cannot run pg_restore (is the PostgreSQL client installed?)")?;
    let mut stdin = child.stdin.take().context("pg_restore has no stdin")?;
    let copied = std::io::copy(&mut plaintext, &mut stdin);
    drop(stdin);
    let status = child.wait()?;
    anyhow::ensure!(status.success(), "pg_restore failed ({status})");
    copied.context("restore interrupted")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_stays_out_of_argv() {
        let (url, pw) =
            split_password("postgres://modula:p%40ss@db.internal:5432/modula?sslmode=require")
                .unwrap();
        assert_eq!(
            url,
            "postgres://modula@db.internal:5432/modula?sslmode=require"
        );
        assert_eq!(pw.as_deref().map(String::as_str), Some("p@ss"));
        let (url, pw) =
            split_password("postgres://modula@localhost/modula?host=/var/run/postgresql").unwrap();
        assert_eq!(
            url,
            "postgres://modula@localhost/modula?host=/var/run/postgresql"
        );
        assert!(pw.is_none());
    }

    #[test]
    fn the_identity_follows_the_master_key() {
        let master = MasterKey::generate().unwrap();
        let a = identity_from_master(&master)
            .unwrap()
            .to_public()
            .to_string();
        let b = identity_from_master(&master)
            .unwrap()
            .to_public()
            .to_string();
        let other = identity_from_master(&MasterKey::generate().unwrap())
            .unwrap()
            .to_public()
            .to_string();
        assert_eq!(a, b);
        assert_ne!(a, other);
    }
}
