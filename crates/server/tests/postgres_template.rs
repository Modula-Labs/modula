//! The Postgres template's start script against real `postgres:17`
//! containers (ignored by default: they need Docker):
//!
//! - a streaming read replica made with `pg_basebackup -R`, then promoted;
//! - WAL-G archiving and a point-in-time restore to file storage, with the
//!   WAL-G binary the release ships (`MODULA_TEST_WALG=/path/to/wal-g-linux-<arch>`).

use std::process::Command;
use std::time::{Duration, Instant};

use modula_id::Id;
use modula_server::templates::POSTGRES_START;

const IMAGE: &str = "postgres:17";

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker").args(args).output().expect("docker");
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

fn psql(container: &str, sql: &str) -> Result<String, String> {
    docker(&[
        "exec",
        container,
        "psql",
        "-U",
        "app",
        "-d",
        "shop",
        "-At",
        "-v",
        "ON_ERROR_STOP=1",
        "--command",
        sql,
    ])
}

/// Retry `sql` until it answers `want` (or panic after `secs`).
fn wait_for(container: &str, sql: &str, want: &str, secs: u64) {
    let start = Instant::now();
    let mut last = Err(String::new());
    while start.elapsed() < Duration::from_secs(secs) {
        last = psql(container, sql);
        if last.as_deref() == Ok(want) {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let logs = docker(&["logs", "--tail", "40", container]).unwrap_or_default();
    panic!("{container}: {sql} never gave {want:?} (last {last:?})\n{logs}");
}

/// Removes the containers and network a test made, however it ends.
struct Cleanup(Vec<String>, Option<String>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for c in &self.0 {
            let _ = docker(&["rm", "-f", "-v", c]);
        }
        if let Some(n) = &self.1 {
            let _ = docker(&["network", "rm", n]);
        }
    }
}

/// `docker run` a template Postgres container (as the node runs it: the
/// image's entrypoint with the start command as its arguments).
fn run(name: &str, network: &str, alias: &str, env: &[(&str, &str)], mounts: &[String]) {
    let mut args: Vec<String> = [
        "run",
        "-d",
        "--name",
        name,
        "--network",
        network,
        "--network-alias",
        alias,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let base = [
        ("POSTGRES_USER", "app"),
        ("POSTGRES_PASSWORD", "app-password"),
        ("POSTGRES_DB", "shop"),
        ("PGDATA", "/var/lib/postgresql/data/pgdata"),
        ("REPLICATION_PASSWORD", "replication-password-0123456789"),
    ];
    for (k, v) in base.iter().chain(env) {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    for m in mounts {
        args.push("-v".into());
        args.push(m.clone());
    }
    args.extend([
        IMAGE.into(),
        "/bin/sh".into(),
        "-c".into(),
        POSTGRES_START.into(),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    docker(&args).expect("docker run");
}

#[test]
#[ignore = "needs Docker"]
fn real_streaming_replica_and_promotion() {
    let id = Id::new();
    let (net, primary, replica) = (
        format!("modula-tpl-{id}"),
        format!("modula-tpl-primary-{id}"),
        format!("modula-tpl-replica-{id}"),
    );
    docker(&["network", "create", &net]).unwrap();
    let _cleanup = Cleanup(vec![primary.clone(), replica.clone()], Some(net.clone()));

    run(&primary, &net, "primary", &[], &[]);
    wait_for(&primary, "SELECT 1", "1", 90);
    // A fresh cluster allows replication through the init script…
    let hba = "host replication modula_replicator all scram-sha-256";
    let count = |c: &str| {
        docker(&[
            "exec",
            c,
            "grep",
            "-cxF",
            hba,
            "/var/lib/postgresql/data/pgdata/pg_hba.conf",
        ])
    };
    assert_eq!(count(&primary).unwrap(), "1");
    // …and a restart (an existing cluster) doesn't add it twice.
    docker(&["restart", &primary]).unwrap();
    wait_for(&primary, "SELECT 1", "1", 90);
    assert_eq!(count(&primary).unwrap(), "1");

    // What the control plane runs on the primary when a replica is added.
    psql(
        &primary,
        "CREATE ROLE modula_replicator WITH REPLICATION LOGIN PASSWORD 'replication-password-0123456789'",
    )
    .unwrap();
    psql(
        &primary,
        "CREATE TABLE orders (id int PRIMARY KEY); INSERT INTO orders SELECT generate_series(1, 100)",
    )
    .unwrap();

    run(
        &replica,
        &net,
        "replica",
        &[
            ("MODULA_REPLICA_OF", "primary"),
            ("MODULA_SERVICE_NAME", "shop-replica"),
        ],
        &[],
    );
    wait_for(&replica, "SELECT pg_is_in_recovery()", "t", 120);
    wait_for(&replica, "SELECT count(*) FROM orders", "100", 60);
    psql(
        &primary,
        "INSERT INTO orders SELECT generate_series(101, 150)",
    )
    .unwrap();
    wait_for(&replica, "SELECT count(*) FROM orders", "150", 30);
    assert!(
        psql(&replica, "INSERT INTO orders VALUES (999)").is_err(),
        "read-only"
    );
    wait_for(
        &primary,
        "SELECT application_name FROM pg_stat_replication WHERE state = 'streaming'",
        "shop-replica",
        30,
    );

    // Promote: a standalone primary that takes writes.
    psql(&replica, "SELECT pg_promote(true, 60)").unwrap();
    wait_for(&replica, "SELECT pg_is_in_recovery()", "f", 60);
    psql(&replica, "INSERT INTO orders VALUES (999)").unwrap();
    // A restart keeps it standalone (no standby.signal, PGDATA is not empty).
    docker(&["restart", &replica]).unwrap();
    wait_for(&replica, "SELECT pg_is_in_recovery()", "f", 90);
    wait_for(&replica, "SELECT count(*) FROM orders", "151", 30);
}

#[test]
#[ignore = "needs Docker and MODULA_TEST_WALG (a wal-g-linux binary of this machine's arch)"]
fn real_walg_archive_and_point_in_time_restore() {
    let walg = std::env::var("MODULA_TEST_WALG").expect("MODULA_TEST_WALG");
    let id = Id::new();
    let (net, source, restored) = (
        format!("modula-walg-{id}"),
        format!("modula-walg-source-{id}"),
        format!("modula-walg-restored-{id}"),
    );
    let storage = std::env::temp_dir().join(format!("modula-walg-{id}"));
    std::fs::create_dir_all(&storage).unwrap();
    docker(&["network", "create", &net]).unwrap();
    let _cleanup = Cleanup(vec![source.clone(), restored.clone()], Some(net.clone()));
    let key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    // As backup::pitr renders them for disk storage, and the node mounts.
    let walg_env = [
        ("WALG_LIBSODIUM_KEY", key),
        ("WALG_LIBSODIUM_KEY_TRANSFORM", "hex"),
        ("WALG_COMPRESSION_METHOD", "lz4"),
        ("WALG_FILE_PREFIX", "/var/lib/modula-wal-g"),
    ];
    let mounts = [
        format!("{walg}:/usr/local/bin/wal-g:ro"),
        format!("{}:/var/lib/modula-wal-g", storage.display()),
    ];
    let mut env = walg_env.to_vec();
    env.push(("MODULA_PITR", "1"));
    run(&source, &net, "source", &env, &mounts);
    wait_for(&source, "SELECT 1", "1", 90);
    wait_for(&source, "SHOW archive_mode", "on", 30);
    psql(
        &source,
        "CREATE TABLE orders (id int PRIMARY KEY); INSERT INTO orders SELECT generate_series(1, 100)",
    )
    .unwrap();

    // A base backup, as the node takes it.
    docker(&[
        "exec",
        "-e",
        "PGHOST=/var/run/postgresql",
        "-e",
        "PGUSER=app",
        "-e",
        "PGDATABASE=shop",
        &source,
        "gosu",
        "postgres",
        "wal-g",
        "backup-push",
        "/var/lib/postgresql/data/pgdata",
    ])
    .expect("backup-push");
    let list = docker(&[
        "exec",
        &source,
        "gosu",
        "postgres",
        "wal-g",
        "backup-list",
        "--json",
        "--detail",
    ])
    .unwrap();
    let listed: serde_json::Value = serde_json::from_str(&list).unwrap();
    let base = listed[0]["backup_name"].as_str().unwrap().to_owned();

    psql(
        &source,
        "INSERT INTO orders SELECT generate_series(101, 150)",
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let at = psql(
        &source,
        "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS+00')",
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    psql(&source, "DELETE FROM orders WHERE id > 10").unwrap();
    psql(&source, "SELECT pg_switch_wal()").unwrap();
    wait_for(
        &source,
        "SELECT last_archived_time > now() - interval '1 minute' FROM pg_stat_archiver",
        "t",
        60,
    );

    // Restore into an empty data directory: base backup, replay to `at`, promote.
    let mut env = walg_env.to_vec();
    env.push(("MODULA_PITR_RESTORE_TIME", &at));
    env.push(("MODULA_PITR_RESTORE_BACKUP", &base));
    run(&restored, &net, "restored", &env, &mounts);
    wait_for(&restored, "SELECT pg_is_in_recovery()", "f", 180);
    wait_for(&restored, "SELECT count(*) FROM orders", "150", 30);
    psql(&restored, "INSERT INTO orders VALUES (999)").unwrap();
    let _ = std::fs::remove_dir_all(storage);
}
