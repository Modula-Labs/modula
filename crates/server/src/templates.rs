//! One-click databases: an image, a volume, generated credentials and
//! connection-string variables built from references, so other services can
//! use `${{postgres.DATABASE_URL}}` and the URLs follow renames and proxies.

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
const POSTGRES_START: &str = concat!(
    "set -e\n",
    "d=/var/lib/postgresql/data/certs\n",
    "mkdir -p \"$d\"\n",
    "if ! openssl x509 -checkend 2592000 -noout -in \"$d/server.crt\" >/dev/null 2>&1; then\n",
    "  openssl req -new -x509 -days 3650 -nodes -newkey ec -pkeyopt ec_paramgen_curve:prime256v1",
    " -subj \"/CN=${MODULA_PRIVATE_DOMAIN:-postgres}\" -keyout \"$d/server.key\" -out \"$d/server.crt\" 2>/dev/null\n",
    "fi\n",
    "if [ \"$(id -u)\" = 0 ]; then chown -R postgres:postgres \"$d\"; fi\n",
    "chmod 600 \"$d/server.key\"\n",
    "exec docker-entrypoint.sh postgres -c ssl=on -c \"ssl_cert_file=$d/server.crt\"",
    " -c \"ssl_key_file=$d/server.key\" -c shared_preload_libraries=pg_stat_statements\n",
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
        start_command: Some(&["/bin/sh", "-c", POSTGRES_START]),
        variables: &[
            ("POSTGRES_USER", "postgres"),
            ("POSTGRES_PASSWORD", SECRET),
            ("POSTGRES_DB", "app"),
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
            "-c",
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
