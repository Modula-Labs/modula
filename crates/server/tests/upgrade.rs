//! Upgrading an instance: a database migrated by an earlier release takes the
//! migrations added since, without touching the ones it already ran.

use modula_id::Id;
use sqlx::PgPool;
use sqlx::migrate::Migrator;

/// The migrations 0.1.10 shipped with, applied as that release did.
async fn as_0_1_10(pool: &PgPool) {
    let dir = std::env::temp_dir().join(format!("modula-migrations-{}", Id::new()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/migrations/0001_init.sql"),
        dir.join("0001_init.sql"),
    )
    .unwrap();
    Migrator::new(dir.as_path())
        .await
        .unwrap()
        .run(pool)
        .await
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_0_1_10_database_upgrades(pool: PgPool) {
    as_0_1_10(&pool).await;
    let ws = Id::new();
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'team')")
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    let server = Id::new();
    sqlx::query(
        "INSERT INTO provisioned_servers (id, workspace_id, provider, name, location, server_type, monthly_price_cents)
         VALUES ($1, $2, 'hetzner', 'worker-1', 'hetzner:fsn1', 'cx22', 1000)",
    )
    .bind(server)
    .bind(ws)
    .execute(&pool)
    .await
    .unwrap();

    modula_server::db::migrate(&pool).await.unwrap();

    let price: i32 =
        sqlx::query_scalar("SELECT monthly_price_cents FROM provisioned_servers WHERE id = $1")
            .bind(server)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        price, 1080,
        "euro cents became USD cents at the default rate"
    );
    sqlx::query(
        "INSERT INTO provisioned_servers (id, workspace_id, provider, name, location, server_type, monthly_price_cents)
         VALUES ($1, $2, 'aws', 'worker-2', 'aws:us-east-1', 't3.small', 1500)",
    )
    .bind(Id::new())
    .bind(ws)
    .execute(&pool)
    .await
    .unwrap();
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, [1, 2]);

    // Starting again changes nothing.
    modula_server::db::migrate(&pool).await.unwrap();
}
