//! One-click databases and buckets: an image, a volume, generated
//! credentials and connection variables built from references, so other
//! services can use `${{postgres.DATABASE_URL}}` and the URLs follow renames
//! and proxies. Cloudflare R2 buckets are not templates (their credentials
//! come from Cloudflare): see `api::buckets`.

use serde::Serialize;
use serde_json::json;

use crate::deploy::config::ServiceConfig;

/// Replaced by a fresh random password when a service is created.
pub const SECRET: &str = "<generated>";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub image: &'static str,
    pub default_name: &'static str,
    /// Variables the Connect dialog shows.
    pub connect: &'static [Connect],
    #[serde(skip)]
    pub port: u16,
    #[serde(skip)]
    pub mount_path: &'static str,
    #[serde(skip)]
    pub start_command: Option<&'static [&'static str]>,
    #[serde(skip)]
    pub variables: &'static [(&'static str, &'static str)],
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Connect {
    pub label: &'static str,
    pub variable: &'static str,
}

const fn urls(private: &'static str, public: &'static str) -> [Connect; 2] {
    [
        Connect {
            label: "Private URL",
            variable: private,
        },
        Connect {
            label: "Public URL",
            variable: public,
        },
    ]
}

/// Postgres with TLS, because the TCP proxy exposes it publicly. A self-signed
/// certificate is made on first start (and again when it nears expiry) next
/// to PGDATA on the volume, never inside it: initdb needs an empty directory.
/// Clients use `sslmode=require` (encrypted, unverified, like Railway); the
/// private network still accepts plain connections. pg_stat_statements is
/// preloaded so the Database tab's slow-query view works once installed.
/// `cluster_name` is the service's name, which a replica's WAL receiver
/// reports as its `application_name` in the primary's `pg_stat_replication`.
///
/// The rest follows the environment the control plane sets:
/// - Read replicas connect as `modula_replicator` (`REPLICATION_PASSWORD`,
///   a role the control plane creates when a replica is added), so pg_hba
///   allows replication for it: appended to an existing cluster's file, or
///   by an init script on first start.
/// - `MODULA_PITR`: WAL archiving with WAL-G (`backup::pitr`).
/// - `MODULA_PITR_RESTORE_TIME`/`_BACKUP`: an empty data directory is first
///   filled from that WAL-G base backup, then WAL is replayed up to that
///   time and the server promotes itself.
/// - `MODULA_REPLICA_OF`: an empty data directory is first filled with
///   `pg_basebackup -R` from that primary, so the server runs as its
///   streaming standby.
pub const POSTGRES_START: &str = concat!(
    "set -e\n",
    "d=/var/lib/postgresql/data/certs\n",
    "mkdir -p \"$d\"\n",
    "if ! openssl x509 -checkend 2592000 -noout -in \"$d/server.crt\" >/dev/null 2>&1; then\n",
    "  openssl req -new -x509 -days 3650 -nodes -newkey ec -pkeyopt ec_paramgen_curve:prime256v1",
    " -subj \"/CN=${MODULA_PRIVATE_DOMAIN:-postgres}\" -keyout \"$d/server.key\" -out \"$d/server.crt\" 2>/dev/null\n",
    "fi\n",
    "root=; if [ \"$(id -u)\" = 0 ]; then root=1; chown -R postgres:postgres \"$d\"; fi\n",
    "chmod 600 \"$d/server.key\"\n",
    "as_pg() { if [ -n \"$root\" ]; then gosu postgres \"$@\"; else \"$@\"; fi; }\n",
    "set -- -c ssl=on -c \"ssl_cert_file=$d/server.crt\" -c \"ssl_key_file=$d/server.key\"",
    " -c shared_preload_libraries=pg_stat_statements -c \"cluster_name=${MODULA_SERVICE_NAME:-postgres}\"\n",
    "hba='host replication modula_replicator all scram-sha-256'\n",
    "if [ -s \"$PGDATA/pg_hba.conf\" ]; then\n",
    "  grep -qxF \"$hba\" \"$PGDATA/pg_hba.conf\" || echo \"$hba\" >>\"$PGDATA/pg_hba.conf\"\n",
    "elif [ -n \"$root\" ]; then\n",
    "  mkdir -p /docker-entrypoint-initdb.d\n",
    "  echo \"echo '$hba' >>\\\"\\$PGDATA/pg_hba.conf\\\"\" >/docker-entrypoint-initdb.d/zz-modula-replication.sh\n",
    "fi\n",
    "if [ -n \"$MODULA_PITR\" ]; then\n",
    "  set -- \"$@\" -c wal_level=replica -c archive_mode=on -c 'archive_command=wal-g wal-push %p' -c archive_timeout=60\n",
    "fi\n",
    "if [ -d /var/lib/modula-wal-g ] && [ -n \"$root\" ]; then chown postgres:postgres /var/lib/modula-wal-g; fi\n",
    "if [ ! -s \"$PGDATA/PG_VERSION\" ] && { [ -n \"$MODULA_PITR_RESTORE_TIME\" ] || [ -n \"$MODULA_REPLICA_OF\" ]; }; then\n",
    "  mkdir -p \"$PGDATA\"\n",
    "  if [ -n \"$root\" ]; then chown postgres:postgres \"$PGDATA\"; fi\n",
    "  chmod 700 \"$PGDATA\"\n",
    "  if [ -n \"$MODULA_PITR_RESTORE_TIME\" ]; then\n",
    "    as_pg wal-g backup-fetch \"$PGDATA\" \"$MODULA_PITR_RESTORE_BACKUP\"\n",
    "    as_pg touch \"$PGDATA/recovery.signal\"\n",
    "  else\n",
    "    until as_pg env PGPASSWORD=\"$REPLICATION_PASSWORD\" pg_basebackup -h \"$MODULA_REPLICA_OF\"",
    " -U modula_replicator -D \"$PGDATA\" -R -X stream -c fast; do\n",
    "      echo 'waiting for the primary'; find \"$PGDATA\" -mindepth 1 -delete; sleep 5\n",
    "    done\n",
    "  fi\n",
    "fi\n",
    "if [ -n \"$MODULA_PITR_RESTORE_TIME\" ]; then\n",
    "  set -- \"$@\" -c 'restore_command=wal-g wal-fetch %f %p'",
    " -c \"recovery_target_time=$MODULA_PITR_RESTORE_TIME\" -c recovery_target_action=promote\n",
    "fi\n",
    "exec docker-entrypoint.sh postgres \"$@\"\n",
);

pub const CATALOG: &[Template] = &[
    Template {
        id: "postgres",
        name: "PostgreSQL",
        description: "Relational database with a persistent volume",
        // Debian, not alpine: it ships the openssl CLI the TLS setup needs.
        image: "postgres:17",
        default_name: "postgres",
        connect: &urls("DATABASE_URL", "DATABASE_PUBLIC_URL"),
        port: 5432,
        mount_path: "/var/lib/postgresql/data",
        start_command: Some(&["/bin/sh", "-c", POSTGRES_START]), // no-shell: allow (fixed script, no user input)
        variables: &[
            ("POSTGRES_USER", "postgres"),
            ("POSTGRES_PASSWORD", SECRET),
            ("POSTGRES_DB", "app"),
            // What read replicas connect with (see POSTGRES_START).
            ("REPLICATION_PASSWORD", SECRET),
            // A subdirectory: the volume root may hold lost+found.
            ("PGDATA", "/var/lib/postgresql/data/pgdata"),
            ("PGHOST", "${{MODULA_PRIVATE_DOMAIN}}"),
            ("PGPORT", "5432"),
            ("PGUSER", "${{POSTGRES_USER}}"),
            ("PGPASSWORD", "${{POSTGRES_PASSWORD}}"),
            ("PGDATABASE", "${{POSTGRES_DB}}"),
            (
                "DATABASE_URL",
                "postgresql://${{PGUSER}}:${{PGPASSWORD}}@${{MODULA_PRIVATE_DOMAIN}}:5432/${{PGDATABASE}}",
            ),
            (
                "DATABASE_PUBLIC_URL",
                "postgresql://${{PGUSER}}:${{PGPASSWORD}}@${{MODULA_TCP_PROXY_DOMAIN}}:${{MODULA_TCP_PROXY_PORT}}/${{PGDATABASE}}?sslmode=require",
            ),
        ],
    },
    Template {
        id: "mysql",
        name: "MySQL",
        description: "Relational database with a persistent volume",
        image: "mysql:8.4",
        default_name: "mysql",
        connect: &urls("MYSQL_URL", "MYSQL_PUBLIC_URL"),
        port: 3306,
        mount_path: "/var/lib/mysql",
        start_command: None,
        variables: &[
            ("MYSQL_ROOT_PASSWORD", SECRET),
            ("MYSQL_DATABASE", "app"),
            ("MYSQLHOST", "${{MODULA_PRIVATE_DOMAIN}}"),
            ("MYSQLPORT", "3306"),
            ("MYSQLUSER", "root"),
            ("MYSQLPASSWORD", "${{MYSQL_ROOT_PASSWORD}}"),
            ("MYSQLDATABASE", "${{MYSQL_DATABASE}}"),
            (
                "MYSQL_URL",
                "mysql://root:${{MYSQL_ROOT_PASSWORD}}@${{MODULA_PRIVATE_DOMAIN}}:3306/${{MYSQL_DATABASE}}",
            ),
            (
                "MYSQL_PUBLIC_URL",
                "mysql://root:${{MYSQL_ROOT_PASSWORD}}@${{MODULA_TCP_PROXY_DOMAIN}}:${{MODULA_TCP_PROXY_PORT}}/${{MYSQL_DATABASE}}",
            ),
        ],
    },
    Template {
        id: "redis",
        name: "Redis",
        description: "In-memory store with append-only persistence",
        image: "redis:7-alpine",
        default_name: "redis",
        connect: &urls("REDIS_URL", "REDIS_PUBLIC_URL"),
        port: 6379,
        mount_path: "/data",
        // The password comes from the environment, so a shell expands it.
        start_command: Some(&[
            "/bin/sh",
            "-c", // no-shell: allow (fixed script; the password comes from env)
            "exec redis-server --requirepass \"$REDIS_PASSWORD\" --appendonly yes --dir /data",
        ]),
        variables: &[
            ("REDIS_PASSWORD", SECRET),
            ("REDISHOST", "${{MODULA_PRIVATE_DOMAIN}}"),
            ("REDISPORT", "6379"),
            ("REDISUSER", "default"),
            (
                "REDIS_URL",
                "redis://default:${{REDIS_PASSWORD}}@${{MODULA_PRIVATE_DOMAIN}}:6379",
            ),
            (
                "REDIS_PUBLIC_URL",
                "redis://default:${{REDIS_PASSWORD}}@${{MODULA_TCP_PROXY_DOMAIN}}:${{MODULA_TCP_PROXY_PORT}}",
            ),
        ],
    },
    Template {
        id: "mongo",
        name: "MongoDB",
        description: "Document database with a persistent volume",
        image: "mongo:8",
        default_name: "mongo",
        connect: &urls("MONGO_URL", "MONGO_PUBLIC_URL"),
        port: 27017,
        mount_path: "/data/db",
        start_command: None,
        variables: &[
            ("MONGO_INITDB_ROOT_USERNAME", "mongo"),
            ("MONGO_INITDB_ROOT_PASSWORD", SECRET),
            ("MONGOHOST", "${{MODULA_PRIVATE_DOMAIN}}"),
            ("MONGOPORT", "27017"),
            ("MONGOUSER", "${{MONGO_INITDB_ROOT_USERNAME}}"),
            ("MONGOPASSWORD", "${{MONGO_INITDB_ROOT_PASSWORD}}"),
            (
                "MONGO_URL",
                "mongodb://${{MONGOUSER}}:${{MONGOPASSWORD}}@${{MODULA_PRIVATE_DOMAIN}}:27017",
            ),
            (
                "MONGO_PUBLIC_URL",
                "mongodb://${{MONGOUSER}}:${{MONGOPASSWORD}}@${{MODULA_TCP_PROXY_DOMAIN}}:${{MODULA_TCP_PROXY_PORT}}",
            ),
        ],
    },
    // rclone's S3 server (MIT, alpine, ~30 MB RAM), serving /data with the
    // one bucket as its only top-level directory. The volume is mounted at
    // /data/bucket, so the bucket exists from the first start and holds all
    // the data. The image's entrypoint is `rclone`, so the start command is
    // its arguments; rclone reads `--auth-key` from RCLONE_AUTH_KEY, a CSV
    // list, hence the quotes around the `id,secret` pair.
    Template {
        id: "bucket",
        name: "Bucket",
        description: "S3-compatible storage on your own servers",
        image: "rclone/rclone:1.75.2@sha256:2687085f718d3c628f7fdfb77c52a1d344332aed543110bb88088f2c70d43eb5",
        default_name: "bucket",
        connect: BUCKET_CONNECT,
        port: 9000,
        mount_path: "/data/bucket",
        start_command: Some(&["serve", "s3", "/data", "--addr", ":9000"]),
        variables: &[
            ("S3_BUCKET", "bucket"),
            ("BUCKET_NAME", "${{S3_BUCKET}}"),
            ("S3_ENDPOINT", "http://${{MODULA_PRIVATE_DOMAIN}}:9000"),
            ("S3_REGION", "us-east-1"),
            ("S3_FORCE_PATH_STYLE", "true"),
            ("AWS_ACCESS_KEY_ID", SECRET),
            ("AWS_SECRET_ACCESS_KEY", SECRET),
            (
                "RCLONE_AUTH_KEY",
                "\"${{AWS_ACCESS_KEY_ID}},${{AWS_SECRET_ACCESS_KEY}}\"",
            ),
        ],
    },
];

/// The S3 variables a bucket (self-hosted or R2) exposes, in Connect order.
pub const BUCKET_CONNECT: &[Connect] = &[
    Connect {
        label: "Endpoint",
        variable: "S3_ENDPOINT",
    },
    Connect {
        label: "Bucket",
        variable: "S3_BUCKET",
    },
    Connect {
        label: "Region",
        variable: "S3_REGION",
    },
    Connect {
        label: "Access key ID",
        variable: "AWS_ACCESS_KEY_ID",
    },
    Connect {
        label: "Secret access key",
        variable: "AWS_SECRET_ACCESS_KEY",
    },
];

pub fn get(id: &str) -> Option<&'static Template> {
    CATALOG.iter().find(|t| t.id == id)
}

impl Template {
    pub fn config(&self) -> ServiceConfig {
        let mut cfg = json!({
            "source": { "type": "image", "image": self.image },
            "port": self.port,
        });
        if let Some(cmd) = self.start_command {
            cfg["startCommand"] = json!(cmd);
        }
        serde_json::from_value(cfg).expect("template configs are valid")
    }

    /// The variables to create, with fresh passwords.
    pub fn variables(&self) -> Vec<(String, String)> {
        self.variables
            .iter()
            .map(|(k, v)| {
                let v = if *v == SECRET {
                    password()
                } else {
                    (*v).to_owned()
                };
                ((*k).to_owned(), v)
            })
            .collect()
    }
}

/// 32 characters from [A-Za-z0-9]: ~190 bits, safe in URLs and shells.
pub fn password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(32);
    while out.len() < 32 {
        let buf: [u8; 64] = modula_crypto::random_bytes().expect("OS random number generator");
        // Rejection sampling keeps the distribution uniform.
        out.extend(
            buf.iter()
                .filter(|b| (**b as usize) < 248)
                .map(|b| ALPHABET[*b as usize % 62] as char)
                .take(32 - out.len()),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_postgres_start_script_parses_and_covers_its_modes() {
        let out = std::process::Command::new("sh") // no-shell: allow (test: syntax-checks a fixed script)
            .args(["-n", "-c", POSTGRES_START]) // no-shell: allow (same)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for part in [
            "host replication modula_replicator all scram-sha-256",
            "/docker-entrypoint-initdb.d/zz-modula-replication.sh",
            "-c wal_level=replica -c archive_mode=on -c 'archive_command=wal-g wal-push %p' -c archive_timeout=60",
            "as_pg wal-g backup-fetch \"$PGDATA\" \"$MODULA_PITR_RESTORE_BACKUP\"",
            "recovery_target_time=$MODULA_PITR_RESTORE_TIME",
            "recovery_target_action=promote",
            "pg_basebackup -h \"$MODULA_REPLICA_OF\" -U modula_replicator -D \"$PGDATA\" -R -X stream",
            "exec docker-entrypoint.sh postgres \"$@\"",
        ] {
            assert!(POSTGRES_START.contains(part), "{part}");
        }
        let pg = get("postgres").unwrap();
        assert!(
            pg.variables
                .iter()
                .any(|(k, v)| *k == "REPLICATION_PASSWORD" && *v == SECRET)
        );
    }

    #[test]
    fn templates_are_valid_and_passwords_fresh() {
        for t in CATALOG {
            t.config().validate().unwrap();
            let vars = t.variables();
            assert!(
                vars.iter().any(|(k, _)| k == t.connect[0].variable),
                "{}",
                t.id
            );
            for (k, v) in &vars {
                crate::deploy::env::validate_value(v).unwrap();
                assert!(!v.contains(SECRET), "{k}");
            }
        }
        let a = password();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(a, password());
    }
}
