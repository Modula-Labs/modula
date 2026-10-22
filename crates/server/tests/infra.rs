mod common;

use modula_crypto::envelope::{FieldAad, MasterKey};
use modula_server::jobs::{self, NewJob};
use modula_server::keyring::Keyring;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn job_lifecycle(pool: PgPool) {
    let id = jobs::enqueue(
        &pool,
        NewJob {
            kind: "t",
            payload: json!({"x": 1}),
            max_attempts: Some(2),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();

    let job = jobs::claim(&pool, "w1").await.unwrap().unwrap();
    assert_eq!((job.id, job.attempts), (id, 1));
    assert!(
        jobs::claim(&pool, "w2").await.unwrap().is_none(),
        "claimed jobs are invisible"
    );

    // A different worker cannot complete someone else's lease.
    assert!(!jobs::complete(&pool, id, "w2").await.unwrap());

    // First failure retries with backoff; it is not claimable until run_at.
    jobs::fail(&pool, &job, "w1", "boom", false).await.unwrap();
    let (status, err): (String, String) =
        sqlx::query_as("SELECT status, last_error FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), err.as_str()), ("queued", "boom"));
    assert!(jobs::claim(&pool, "w1").await.unwrap().is_none());

    // Second attempt fails → dead (max_attempts = 2).
    sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let job = jobs::claim(&pool, "w1").await.unwrap().unwrap();
    assert_eq!(job.attempts, 2);
    jobs::fail(&pool, &job, "w1", "boom again", false)
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "dead");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn job_dedupe_and_reap(pool: PgPool) {
    let k = Some("reconcile:node-1");
    let a = jobs::enqueue(
        &pool,
        NewJob {
            kind: "r",
            dedupe_key: k,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let b = jobs::enqueue(
        &pool,
        NewJob {
            kind: "r",
            dedupe_key: k,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(a.is_some());
    assert!(
        b.is_none(),
        "second pending job with the same key is dropped"
    );

    let job = jobs::claim(&pool, "w1").await.unwrap().unwrap();
    // Simulate a crashed worker whose lease expired.
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second' WHERE id = $1")
        .bind(job.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(jobs::reap_expired(&pool).await.unwrap(), 1);
    let again = jobs::claim(&pool, "w2").await.unwrap().unwrap();
    assert_eq!((again.id, again.attempts), (job.id, 2));
    assert!(jobs::complete(&pool, again.id, "w2").await.unwrap());
    assert!(
        !jobs::complete(&pool, again.id, "w2").await.unwrap(),
        "completion is idempotent-safe"
    );

    // Once finished, the key is free again.
    let c = jobs::enqueue(
        &pool,
        NewJob {
            kind: "r",
            dedupe_key: k,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(c.is_some());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn worker_runs_handlers_and_kills_unknown_kinds(pool: PgPool) {
    let state = common::state(pool.clone());
    let ok = jobs::enqueue(
        &pool,
        NewJob {
            kind: "known",
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();
    let unknown = jobs::enqueue(
        &pool,
        NewJob {
            kind: "mystery",
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();
    let stop = tokio_util::sync::CancellationToken::new();
    let handle = tokio::spawn(jobs::run_worker(
        state,
        jobs::Registry::default().on("known", |_, _| async { Ok(()) }),
        "w".into(),
        stop.clone(),
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let done: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE status IN ('succeeded', 'dead')")
                .fetch_one(&pool)
                .await
                .unwrap();
        if done == 2 || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    stop.cancel();
    handle.await.unwrap();
    let status = |id: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT status FROM jobs WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(status(ok).await, "succeeded");
    assert_eq!(status(unknown).await, "dead");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn keyring_persists_and_binds_keys(pool: PgPool) {
    let kek = MasterKey::generate().unwrap();
    let encoded = kek.encode();
    let ws = Uuid::now_v7();
    let aad = FieldAad {
        scope: ws,
        table: "variables",
        column: "value",
        row: "row-1",
    };

    let ring = Keyring::new(kek);
    let sealed = ring
        .seal(&pool, b"DATABASE_URL=postgres://...", aad)
        .await
        .unwrap();

    // A fresh process with the same master key decrypts via the stored wrapped DEK.
    let ring2 = Keyring::new(MasterKey::from_encoded(&encoded).unwrap());
    assert_eq!(
        &*ring2.open(&pool, &sealed, aad).await.unwrap(),
        b"DATABASE_URL=postgres://..."
    );

    // Moving the ciphertext to another row fails.
    let moved = FieldAad {
        row: "row-2",
        ..aad
    };
    assert!(ring2.open(&pool, &sealed, moved).await.is_err());

    // A different master key refuses to use the stored DEK.
    let wrong = Keyring::new(MasterKey::generate().unwrap());
    assert!(wrong.open(&pool, &sealed, aad).await.is_err());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn refuses_schema_newer_than_binary(pool: PgPool) {
    modula_server::db::migrate(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (99990101000000, 'from the future', true, '\\x00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let err = modula_server::db::migrate(&pool)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("newer than this binary"), "{err}");
}

/// Runs against a real Redis when MODULA_TEST_REDIS_URL is set (CI sets it).
#[tokio::test]
async fn redis_rate_limiter() {
    let Ok(url) = std::env::var("MODULA_TEST_REDIS_URL") else {
        eprintln!("skipping: MODULA_TEST_REDIS_URL not set");
        return;
    };
    use modula_server::ratelimit::{Limit, RateLimiter};
    let client = redis::Client::open(url).unwrap();
    let rl = RateLimiter::Redis(redis::aio::ConnectionManager::new(client).await.unwrap());
    let key = Uuid::now_v7().to_string();
    let lim = Limit::per(2, std::time::Duration::from_millis(400));
    rl.check("test", &key, lim).await.unwrap();
    rl.check("test", &key, lim).await.unwrap();
    assert!(rl.check("test", &key, lim).await.is_err());
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    rl.check("test", &key, lim).await.unwrap();
}
