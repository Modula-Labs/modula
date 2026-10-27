//! `modula connect`: a private tunnel to a database, then its native
//! client (`psql`, `mysql`, `redis-cli`, `mongosh`), or just the tunnel.

use anyhow::Context;
use url::Url;

use crate::api::{Api, DbCredentials, api_error};
use crate::config::Link;
use crate::resolve;
use crate::tunnel::{self, Target};

pub struct ConnectArgs {
    pub service: Option<String>,
    pub environment: Option<String>,
    pub project: Option<String>,
    pub tunnel_only: bool,
    pub port: Option<u16>,
    /// Print the connection string with its password (else masked).
    pub show_password: bool,
}

/// Database engines with a known client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Postgres,
    Mysql,
    Redis,
    Mongo,
}

impl Engine {
    pub fn from_template(t: &str) -> Option<Self> {
        Some(match t {
            "postgres" | "postgresql" => Self::Postgres,
            "mysql" | "mariadb" => Self::Mysql,
            "redis" => Self::Redis,
            "mongo" | "mongodb" => Self::Mongo,
            _ => return None,
        })
    }

    fn scheme(self) -> &'static str {
        match self {
            Self::Postgres => "postgresql",
            Self::Mysql => "mysql",
            Self::Redis => "redis",
            Self::Mongo => "mongodb",
        }
    }

    fn client(self) -> &'static str {
        match self {
            Self::Postgres => "psql",
            Self::Mysql => "mysql",
            Self::Redis => "redis-cli",
            Self::Mongo => "mongosh",
        }
    }

    fn install_hint(self) -> &'static str {
        match self {
            Self::Postgres => "brew install libpq, or apt install postgresql-client",
            Self::Mysql => "brew install mysql-client, or apt install mysql-client",
            Self::Redis => "brew install redis, or apt install redis-tools",
            Self::Mongo => "see https://www.mongodb.com/try/download/shell",
        }
    }
}

/// A connection string to the local end of the tunnel. Prefers the
/// database's own private URL (keeps options like `authSource`), re-pointed
/// at 127.0.0.1.
pub fn local_url(engine: Option<Engine>, creds: &DbCredentials, local_port: u16) -> Option<Url> {
    if let Some(mut u) = creds
        .private_url
        .as_deref()
        .and_then(|u| Url::parse(u).ok())
    {
        u.set_host(Some("127.0.0.1")).ok()?;
        u.set_port(Some(local_port)).ok()?;
        return Some(u);
    }
    let engine = engine?;
    let mut u = Url::parse(&format!("{}://127.0.0.1:{local_port}/", engine.scheme())).ok()?;
    if !creds.username.is_empty() {
        u.set_username(&creds.username).ok()?;
    }
    if !creds.password.is_empty() {
        u.set_password(Some(&creds.password)).ok()?;
    }
    if let Some(db) = creds.database.as_deref().filter(|d| !d.is_empty())
        && engine != Engine::Redis
    {
        u.set_path(db);
    }
    Some(u)
}

/// The URL with the password masked, for printing.
pub fn redacted(u: &Url) -> String {
    let mut u = u.clone();
    if u.password().is_some() {
        let _ = u.set_password(Some("****"));
    }
    u.to_string()
}

/// How to start the native client: program, args, extra environment.
/// Passwords go through the environment where the client supports it, so
/// they don't show up in the process list.
pub fn client_command(
    engine: Engine,
    creds: &DbCredentials,
    url: &Url,
    local_port: u16,
) -> (String, Vec<String>, Vec<(String, String)>) {
    let port = local_port.to_string();
    let user = if creds.username.is_empty() {
        url.username().to_owned()
    } else {
        creds.username.clone()
    };
    let password = if creds.password.is_empty() {
        url.password().unwrap_or_default().to_owned()
    } else {
        creds.password.clone()
    };
    let mut env = Vec::new();
    let args = match engine {
        Engine::Postgres => {
            let mut no_pw = url.clone();
            let _ = no_pw.set_password(None);
            if !password.is_empty() {
                env.push(("PGPASSWORD".into(), password));
            }
            vec![no_pw.to_string()]
        }
        Engine::Mysql => {
            if !password.is_empty() {
                env.push(("MYSQL_PWD".into(), password));
            }
            let mut a = vec![
                "--protocol=TCP".into(),
                "-h".into(),
                "127.0.0.1".into(),
                "-P".into(),
                port,
            ];
            if !user.is_empty() {
                a.extend(["-u".into(), user]);
            }
            let db = url.path().trim_start_matches('/');
            if !db.is_empty() {
                a.push(db.to_owned());
            }
            a
        }
        Engine::Redis => {
            if !password.is_empty() {
                env.push(("REDISCLI_AUTH".into(), password));
            }
            let mut a = vec!["-h".into(), "127.0.0.1".into(), "-p".into(), port];
            if !user.is_empty() && user != "default" {
                a.extend(["--user".into(), user]);
            }
            a
        }
        // mongosh reads credentials only from its arguments.
        Engine::Mongo => vec![url.to_string()],
    };
    (engine.client().to_owned(), args, env)
}

pub async fn run(api: &Api, link: Option<&Link>, args: ConnectArgs) -> anyhow::Result<i32> {
    let wanted_project = args.project.as_deref().or(link.map(|l| l.project.as_str()));
    let project = resolve::project(api, wanted_project).await?;
    let env_name = args
        .environment
        .as_deref()
        .or(link.and_then(|l| l.environment.as_deref()));
    let env = resolve::pick_environment(&project.environments, env_name)?;
    let service = resolve::pick_service(&project.services, args.service.as_deref())?;
    let instance = resolve::instance_id(&project, service, env)?;
    let engine = service.template.as_deref().and_then(Engine::from_template);

    // With the Access credentials as renewed by the API client.
    let login = crate::config::Login {
        access: api.access(),
        ..api.login.clone()
    };
    let target = Target::new(&login, &instance, None)?;
    target.check().await.with_context(|| {
        format!(
            "cannot open a tunnel to {} in {}/{}",
            service.name, project.name, env.name
        )
    })?;
    let listener = tunnel::listen(args.port).await?;
    let local_port = listener.local_addr()?.port();
    let tunnels = tokio::spawn(tunnel::serve(listener, target, args.tunnel_only));

    let creds = match api
        .get::<DbCredentials>(&format!(
            "/api/service-instances/{instance}/database/credentials"
        ))
        .await
    {
        Ok(c) => Some(c),
        // Not a database (409), or not allowed to read its credentials: the
        // tunnel alone is still useful.
        Err(e) if api_error(&e).is_some_and(|a| matches!(a.status, 403 | 404 | 409)) => {
            eprintln!("Couldn't read {}'s credentials: {e:#}", service.name);
            None
        }
        Err(e) => return Err(e),
    };
    let url = creds
        .as_ref()
        .and_then(|c| local_url(engine, c, local_port));

    let target_label = format!("{} ({}/{})", service.name, project.name, env.name);
    let client = match (args.tunnel_only, engine, &creds, &url) {
        (false, Some(engine), Some(creds), Some(url)) => {
            Some(client_command(engine, creds, url, local_port))
        }
        _ => None,
    };
    let Some((program, argv, extra_env)) = client else {
        if !args.tunnel_only {
            eprintln!(
                "No database client for {} (or its credentials aren't available); keeping the tunnel open instead.",
                service.name
            );
        }
        println!("Tunnel to {target_label} open on 127.0.0.1:{local_port}");
        match &url {
            Some(u) if args.show_password => println!("Connection string: {u}"),
            Some(u) => println!("Connection string: {}", redacted(u)),
            None => println!(
                "Connect your client to host 127.0.0.1, port {local_port} with the database's credentials."
            ),
        }
        println!("Press Ctrl-C to close it.");
        tokio::signal::ctrl_c().await?;
        tunnels.abort();
        return Ok(0);
    };

    eprintln!("Connecting to {target_label} through a private tunnel (127.0.0.1:{local_port})…");
    // Ctrl-C belongs to the client (psql cancels the query); don't die on it.
    let ignore_interrupts = tokio::spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });
    let status = tokio::process::Command::new(&program)
        .args(&argv)
        .envs(extra_env)
        .status()
        .await;
    ignore_interrupts.abort();
    tunnels.abort();
    match status {
        Ok(s) => Ok(s.code().unwrap_or(1)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let engine = engine.expect("a client implies an engine");
            anyhow::bail!(
                "{program} is not installed ({}). Or run with --tunnel-only and use any client.",
                engine.install_hint()
            )
        }
        Err(e) => Err(e).with_context(|| format!("cannot start {program}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(url: Option<&str>) -> DbCredentials {
        DbCredentials {
            username: "postgres".into(),
            password: "s3cr/t".into(),
            database: Some("app".into()),
            private_url: url.map(Into::into),
        }
    }

    #[test]
    fn private_urls_are_repointed_at_the_tunnel() {
        let c = creds(Some(
            "mongodb://root:pw@mongo.modula.internal:27017/app?authSource=admin",
        ));
        let u = local_url(Some(Engine::Mongo), &c, 61810).unwrap();
        assert_eq!(
            u.as_str(),
            "mongodb://root:pw@127.0.0.1:61810/app?authSource=admin"
        );
        assert_eq!(
            redacted(&u),
            "mongodb://root:****@127.0.0.1:61810/app?authSource=admin"
        );
    }

    #[test]
    fn urls_are_built_from_parts_when_needed() {
        let u = local_url(Some(Engine::Postgres), &creds(None), 5433).unwrap();
        assert_eq!(
            u.as_str(),
            "postgresql://postgres:s3cr%2Ft@127.0.0.1:5433/app"
        );
        assert!(local_url(None, &creds(None), 1).is_none());
    }

    #[test]
    fn passwords_go_through_the_environment() {
        let c = creds(None);
        let u = local_url(Some(Engine::Postgres), &c, 5433).unwrap();
        let (prog, args, env) = client_command(Engine::Postgres, &c, &u, 5433);
        assert_eq!(prog, "psql");
        assert_eq!(args, ["postgresql://postgres@127.0.0.1:5433/app"]);
        assert_eq!(env, [("PGPASSWORD".to_owned(), "s3cr/t".to_owned())]);

        let (prog, args, env) = client_command(Engine::Mysql, &c, &u, 3307);
        assert_eq!(prog, "mysql");
        assert_eq!(
            args,
            [
                "--protocol=TCP",
                "-h",
                "127.0.0.1",
                "-P",
                "3307",
                "-u",
                "postgres",
                "app"
            ]
        );
        assert_eq!(env[0].0, "MYSQL_PWD");

        let redis = DbCredentials {
            username: "default".into(),
            password: "pw".into(),
            ..Default::default()
        };
        let u = local_url(Some(Engine::Redis), &redis, 6380).unwrap();
        let (prog, args, env) = client_command(Engine::Redis, &redis, &u, 6380);
        assert_eq!(prog, "redis-cli");
        assert_eq!(args, ["-h", "127.0.0.1", "-p", "6380"]);
        assert_eq!(env, [("REDISCLI_AUTH".to_owned(), "pw".to_owned())]);
    }

    #[test]
    fn credentials_without_a_database_parse() {
        let c: DbCredentials = serde_json::from_str(
            r#"{"template":"redis","username":"default","password":"pw","database":null,"privateUrl":"redis://default:pw@cache.modula.internal:6379"}"#,
        )
        .unwrap();
        assert_eq!(c.database, None);
        let u = local_url(Some(Engine::Redis), &c, 6380).unwrap();
        assert_eq!(u.as_str(), "redis://default:pw@127.0.0.1:6380");
    }

    #[test]
    fn templates_map_to_engines() {
        assert_eq!(Engine::from_template("postgres"), Some(Engine::Postgres));
        assert_eq!(Engine::from_template("mongo"), Some(Engine::Mongo));
        assert_eq!(Engine::from_template("nginx"), None);
    }
}
