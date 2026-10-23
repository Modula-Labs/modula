//! The dump and restore command of each database engine, as argv run inside
//! the database's own container (its tools, its version). Credentials come
//! from the container's environment, set by the database template, and are
//! passed as environment variables wherever the tool supports it.

use modula_proto::node::v1::DatabaseEngine;

use crate::runtime::Exec;

/// The postgres images' Unix socket: local connections need no password.
const PG_SOCKET: &str = "--host=/var/run/postgresql";

fn var<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn exec(cmd: &[&str], extra: Vec<String>, env: Vec<String>) -> Exec {
    Exec {
        cmd: cmd.iter().map(|s| (*s).to_owned()).chain(extra).collect(),
        env,
    }
}

/// `--username` and `--dbname` for postgres, as the image's entrypoint creates them.
fn pg_target(env: &[(String, String)]) -> (Vec<String>, Vec<String>) {
    let user = var(env, "POSTGRES_USER").unwrap_or("postgres");
    let db = var(env, "POSTGRES_DB").unwrap_or(user);
    let password = var(env, "POSTGRES_PASSWORD")
        .map(|p| format!("PGPASSWORD={p}"))
        .into_iter()
        .collect();
    (
        vec![format!("--username={user}"), format!("--dbname={db}")],
        password,
    )
}

fn mysql_password(env: &[(String, String)]) -> Vec<String> {
    var(env, "MYSQL_ROOT_PASSWORD")
        .map(|p| format!("MYSQL_PWD={p}"))
        .into_iter()
        .collect()
}

/// mongodump/mongorestore take no password from the environment; the
/// arguments are only visible inside the database's own container.
fn mongo_auth(env: &[(String, String)]) -> Vec<String> {
    match (
        var(env, "MONGO_INITDB_ROOT_USERNAME"),
        var(env, "MONGO_INITDB_ROOT_PASSWORD"),
    ) {
        (Some(user), Some(password)) => vec![
            format!("--username={user}"),
            format!("--password={password}"),
            "--authenticationDatabase=admin".into(),
        ],
        _ => vec![],
    }
}

/// The command that writes a full dump to stdout.
pub fn dump(engine: DatabaseEngine, env: &[(String, String)]) -> anyhow::Result<Exec> {
    Ok(match engine {
        DatabaseEngine::EnginePostgres => {
            let (target, password) = pg_target(env);
            exec(&["pg_dump", "--format=custom", PG_SOCKET], target, password)
        }
        DatabaseEngine::EngineMysql => exec(
            &[
                "mysqldump",
                "--user=root",
                "--single-transaction",
                "--routines",
                "--triggers",
                "--all-databases",
            ],
            vec![],
            mysql_password(env),
        ),
        // A snapshot over the replication protocol, written to stdout.
        DatabaseEngine::EngineRedis => exec(
            &["redis-cli", "--no-auth-warning", "--rdb", "-"],
            vec![],
            var(env, "REDIS_PASSWORD")
                .map(|p| format!("REDISCLI_AUTH={p}"))
                .into_iter()
                .collect(),
        ),
        DatabaseEngine::EngineMongo => exec(
            &["mongodump", "--archive", "--gzip"],
            mongo_auth(env),
            vec![],
        ),
        DatabaseEngine::EngineUnspecified => anyhow::bail!("unknown database engine"),
    })
}

/// The command that reads a dump from stdin and replaces the current data.
pub fn restore(engine: DatabaseEngine, env: &[(String, String)]) -> anyhow::Result<Exec> {
    Ok(match engine {
        // One transaction: a dump that fails halfway leaves the data as it was.
        DatabaseEngine::EnginePostgres => {
            let (target, password) = pg_target(env);
            exec(
                &[
                    "pg_restore",
                    "--clean",
                    "--if-exists",
                    "--single-transaction",
                    PG_SOCKET,
                ],
                target,
                password,
            )
        }
        DatabaseEngine::EngineMysql => exec(&["mysql", "--user=root"], vec![], mysql_password(env)),
        DatabaseEngine::EngineMongo => exec(
            &["mongorestore", "--archive", "--gzip", "--drop"],
            mongo_auth(env),
            vec![],
        ),
        // Replacing the RDB file under a running server with append-only
        // persistence is not safe without stopping it; not supported yet.
        DatabaseEngine::EngineRedis => {
            anyhow::bail!("restoring Redis backups is not supported yet")
        }
        DatabaseEngine::EngineUnspecified => anyhow::bail!("unknown database engine"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn credentials_come_from_the_container_environment() {
        let pg = env(&[
            ("POSTGRES_USER", "app"),
            ("POSTGRES_PASSWORD", "s3cret"),
            ("POSTGRES_DB", "shop"),
        ]);
        let d = dump(DatabaseEngine::EnginePostgres, &pg).unwrap();
        assert_eq!(
            d.cmd,
            [
                "pg_dump",
                "--format=custom",
                PG_SOCKET,
                "--username=app",
                "--dbname=shop"
            ]
        );
        assert_eq!(d.env, ["PGPASSWORD=s3cret"]);
        // Defaults follow the image: user postgres, database named after it.
        let d = dump(DatabaseEngine::EnginePostgres, &[]).unwrap();
        assert!(
            d.cmd
                .ends_with(&["--username=postgres".into(), "--dbname=postgres".into()])
        );

        let my = dump(
            DatabaseEngine::EngineMysql,
            &env(&[("MYSQL_ROOT_PASSWORD", "pw")]),
        )
        .unwrap();
        assert_eq!(my.env, ["MYSQL_PWD=pw"]);
        assert!(!my.cmd.iter().any(|a| a.contains("pw")), "never in argv");

        let r = dump(
            DatabaseEngine::EngineRedis,
            &env(&[("REDIS_PASSWORD", "pw")]),
        )
        .unwrap();
        assert_eq!(r.env, ["REDISCLI_AUTH=pw"]);
        assert!(restore(DatabaseEngine::EngineRedis, &[]).is_err());

        let m = restore(
            DatabaseEngine::EngineMongo,
            &env(&[
                ("MONGO_INITDB_ROOT_USERNAME", "root"),
                ("MONGO_INITDB_ROOT_PASSWORD", "pw"),
            ]),
        )
        .unwrap();
        assert!(m.cmd.contains(&"--drop".into()));
        assert!(m.cmd.contains(&"--authenticationDatabase=admin".into()));
    }
}
