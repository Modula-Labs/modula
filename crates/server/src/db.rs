use std::time::Duration;

use anyhow::Context;
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(10)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(300))
        .connect(url)
        .await
        .context("cannot connect to Postgres")
}

/// Apply embedded migrations. sqlx takes a Postgres advisory lock, so concurrent
/// starts are safe. Refuses to start against a schema newer than this binary
/// (e.g. after a rollback to an older release).
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    check_not_newer(pool).await?;
    MIGRATOR
        .run(pool)
        .await
        .context("database migration failed")
}

async fn check_not_newer(pool: &PgPool) -> anyhow::Result<()> {
    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    if !table_exists {
        return Ok(());
    }
    let applied: Option<i64> =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(pool)
            .await?;
    let known = MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0);
    if let Some(applied) = applied
        && applied > known
    {
        anyhow::bail!(
            "database schema version {applied} is newer than this binary supports ({known}); \
             refusing to start. Upgrade modula-server instead of downgrading."
        );
    }
    Ok(())
}
