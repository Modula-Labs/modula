//! The app template gallery: one-click apps made of several services (the
//! app and the databases it needs), like Railway's templates. Database
//! members reuse the database templates in [`crate::templates`]; the others
//! are images pinned by digest. Services reference each other by role
//! (`${{postgres.PGHOST}}`); the references are rewritten to the names the
//! services get when they are created.

use serde::Serialize;
use serde_json::json;

use crate::deploy::config::ServiceConfig;
use crate::deploy::refs;
use crate::templates::{self, SECRET, Template};

/// Replaced by 64 fresh random characters (keys that need more than [`SECRET`]).
pub const SECRET_LONG: &str = "<generated:64>";

/// The public URL of the service's generated domain.
const PUBLIC_URL: &str = "https://${{MODULA_PUBLIC_DOMAIN}}";

#[derive(Debug)]
pub struct App {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub docs_url: &'static str,
    /// Created in this order (databases first, so they deploy first).
    pub services: &'static [AppService],
}

#[derive(Debug)]
pub struct AppService {
    /// The default service name, and how the app's other services reference it.
    pub role: &'static str,
    /// A database template (see `templates.rs`); the fields below are unused then.
    pub template: Option<&'static str>,
    pub image: &'static str,
    pub port: u16,
    pub mount_path: Option<&'static str>,
    pub start_command: Option<&'static [&'static str]>,
    pub healthcheck_path: Option<&'static str>,
    pub variables: &'static [(&'static str, &'static str)],
    /// Gets a generated domain (the app's web service).
    pub domain: bool,
}

const IMAGE: AppService = AppService {
    role: "",
    template: None,
    image: "",
    port: 0,
    mount_path: None,
    start_command: None,
    healthcheck_path: None,
    variables: &[],
    domain: false,
};

const fn database(template: &'static str) -> AppService {
    AppService {
        role: template,
        template: Some(template),
        ..IMAGE
    }
}

/// ClickHouse tuned for a small server, as Plausible's own compose file
/// does: fewer system log tables, a smaller mark cache and single-threaded
/// queries. The config files are written before the image's entrypoint runs.
const CLICKHOUSE_START: &str = concat!(
    "set -e\n",
    "cat > /etc/clickhouse-server/config.d/modula.xml <<'EOF'\n",
    "<clickhouse>\n",
    "  <logger><level>warning</level><console>true</console></logger>\n",
    "  <mark_cache_size>524288000</mark_cache_size>\n",
    "  <query_log replace=\"1\"><database>system</database><table>query_log</table>",
    "<flush_interval_milliseconds>7500</flush_interval_milliseconds>",
    "<engine>ENGINE = MergeTree PARTITION BY event_date ORDER BY (event_time)",
    " TTL event_date + interval 30 day SETTINGS ttl_only_drop_parts=1</engine></query_log>\n",
    "  <metric_log remove=\"remove\"/><asynchronous_metric_log remove=\"remove\"/>",
    "<query_thread_log remove=\"remove\"/><text_log remove=\"remove\"/><trace_log remove=\"remove\"/>",
    "<session_log remove=\"remove\"/><part_log remove=\"remove\"/>\n",
    "</clickhouse>\n",
    "EOF\n",
    "cat > /etc/clickhouse-server/users.d/modula.xml <<'EOF'\n",
    "<clickhouse><profiles><default><max_threads>1</max_threads><max_block_size>8192</max_block_size>",
    "<max_download_threads>1</max_download_threads><input_format_parallel_parsing>0</input_format_parallel_parsing>",
    "<output_format_parallel_formatting>0</output_format_parallel_formatting></default></profiles></clickhouse>\n",
    "EOF\n",
    "exec /entrypoint.sh\n",
);

pub const GALLERY: &[App] = &[
    // https://docs.n8n.io/hosting/configuration/environment-variables/
    App {
        id: "n8n",
        name: "n8n",
        description: "Workflow automation with a visual editor and hundreds of integrations",
        category: "Automation",
        docs_url: "https://docs.n8n.io/hosting/",
        services: &[
            database("postgres"),
            AppService {
                role: "n8n",
                image: "n8nio/n8n:2.44.6@sha256:d02752c6ab446ae9db8deec2f9cc887c378f77cdcad31650814df936b7386dfb",
                port: 5678,
                mount_path: Some("/home/node/.n8n"),
                healthcheck_path: Some("/healthz"),
                domain: true,
                variables: &[
                    ("N8N_PORT", "5678"),
                    ("N8N_HOST", "${{MODULA_PUBLIC_DOMAIN}}"),
                    ("N8N_PROTOCOL", "https"),
                    ("WEBHOOK_URL", "https://${{MODULA_PUBLIC_DOMAIN}}/"),
                    // Behind the edge proxy: trust one X-Forwarded-For hop.
                    ("N8N_PROXY_HOPS", "1"),
                    // Encrypts stored credentials; losing it loses them.
                    ("N8N_ENCRYPTION_KEY", SECRET),
                    ("GENERIC_TIMEZONE", "UTC"),
                    ("DB_TYPE", "postgresdb"),
                    ("DB_POSTGRESDB_HOST", "${{postgres.PGHOST}}"),
                    ("DB_POSTGRESDB_PORT", "5432"),
                    ("DB_POSTGRESDB_DATABASE", "${{postgres.PGDATABASE}}"),
                    ("DB_POSTGRESDB_USER", "${{postgres.PGUSER}}"),
                    ("DB_POSTGRESDB_PASSWORD", "${{postgres.PGPASSWORD}}"),
                ],
                ..IMAGE
            },
        ],
    },
    // https://github.com/plausible/community-edition (compose.yml, wiki/configuration)
    App {
        id: "plausible",
        name: "Plausible Analytics",
        description: "Privacy-friendly, cookie-free web analytics (Community Edition)",
        category: "Analytics",
        docs_url: "https://github.com/plausible/community-edition/wiki/configuration",
        services: &[
            database("postgres"),
            AppService {
                role: "clickhouse",
                image: "clickhouse/clickhouse-server:24.12-alpine@sha256:cd450891db46cc6ffe313ca2b0fb7dbfb897a6873ca74a724cbe050a2cf62621",
                port: 8123,
                mount_path: Some("/var/lib/clickhouse"),
                start_command: Some(&["/bin/sh", "-c", CLICKHOUSE_START]), // no-shell: allow (fixed script, no user input)
                healthcheck_path: Some("/ping"),
                variables: &[
                    ("CLICKHOUSE_DB", "plausible_events_db"),
                    ("CLICKHOUSE_USER", "plausible"),
                    ("CLICKHOUSE_PASSWORD", SECRET),
                    (
                        "CLICKHOUSE_URL",
                        "http://${{CLICKHOUSE_USER}}:${{CLICKHOUSE_PASSWORD}}@${{MODULA_PRIVATE_DOMAIN}}:8123/${{CLICKHOUSE_DB}}",
                    ),
                ],
                ..IMAGE
            },
            AppService {
                role: "plausible",
                image: "ghcr.io/plausible/community-edition:v3.2.1@sha256:33e60bfb40f2df5da00f8753b76fad04f67dba3abe6d73eb516e440e3fb62985",
                port: 8000,
                mount_path: Some("/var/lib/plausible"),
                start_command: Some(&[
                    "/bin/sh",
                    "-c", // no-shell: allow (fixed script, no user input)
                    "/entrypoint.sh db createdb && /entrypoint.sh db migrate && exec /entrypoint.sh run",
                ]),
                healthcheck_path: Some("/api/health"),
                domain: true,
                variables: &[
                    ("BASE_URL", PUBLIC_URL),
                    // At least 64 bytes.
                    ("SECRET_KEY_BASE", SECRET_LONG),
                    ("HTTP_PORT", "8000"),
                    ("TMPDIR", "/var/lib/plausible/tmp"),
                    (
                        "DATABASE_URL",
                        "postgres://${{postgres.PGUSER}}:${{postgres.PGPASSWORD}}@${{postgres.PGHOST}}:5432/${{postgres.PGDATABASE}}",
                    ),
                    ("CLICKHOUSE_DATABASE_URL", "${{clickhouse.CLICKHOUSE_URL}}"),
                ],
                ..IMAGE
            },
        ],
    },
    // https://umami.is/docs/environment-variables
    App {
        id: "umami",
        name: "Umami",
        description: "Simple, fast, privacy-focused website analytics",
        category: "Analytics",
        docs_url: "https://umami.is/docs/install",
        services: &[
            database("postgres"),
            AppService {
                role: "umami",
                image: "ghcr.io/umami-software/umami:3.4.1@sha256:e69d1a7932e9dab0fc523edc7be1f3a53f3c261f57856fc2167ae74a8fc022e8",
                port: 3000,
                healthcheck_path: Some("/api/heartbeat"),
                domain: true,
                variables: &[
                    // Plain on the private network (Prisma would otherwise
                    // try the database's self-signed certificate).
                    ("DATABASE_URL", "${{postgres.DATABASE_URL}}?sslmode=disable"),
                    ("APP_SECRET", SECRET),
                ],
                ..IMAGE
            },
        ],
    },
    // https://www.metabase.com/docs/latest/installation-and-operation/configuring-application-database
    App {
        id: "metabase",
        name: "Metabase",
        description: "Business intelligence: dashboards and questions over your databases",
        category: "Business intelligence",
        docs_url: "https://www.metabase.com/docs/latest/installation-and-operation/running-metabase-on-docker",
        services: &[
            database("postgres"),
            AppService {
                role: "metabase",
                image: "metabase/metabase:v0.64.3.4@sha256:7804ce4edfa217dd08a896821f57b12a9a392d944bd3797a55520d51c76232e3",
                port: 3000,
                healthcheck_path: Some("/api/health"),
                domain: true,
                variables: &[
                    ("MB_DB_TYPE", "postgres"),
                    ("MB_DB_HOST", "${{postgres.PGHOST}}"),
                    ("MB_DB_PORT", "5432"),
                    ("MB_DB_DBNAME", "${{postgres.PGDATABASE}}"),
                    ("MB_DB_USER", "${{postgres.PGUSER}}"),
                    ("MB_DB_PASS", "${{postgres.PGPASSWORD}}"),
                    ("MB_SITE_URL", PUBLIC_URL),
                ],
                ..IMAGE
            },
        ],
    },
    // https://github.com/louislam/uptime-kuma/wiki/Environment-Variables
    App {
        id: "uptime-kuma",
        name: "Uptime Kuma",
        description: "Uptime monitoring with status pages and notifications",
        category: "Monitoring",
        docs_url: "https://github.com/louislam/uptime-kuma/wiki",
        services: &[AppService {
            role: "uptime-kuma",
            image: "louislam/uptime-kuma:2.5.7@sha256:4b95dbe7ac5343b768f8e04e3035ccd59997872b1a35ab9b0f082d2cc279070e",
            port: 3001,
            // SQLite (the default) lives here.
            mount_path: Some("/app/data"),
            domain: true,
            // No health check: `/` redirects to the dashboard.
            ..IMAGE
        }],
    },
    // https://ghost.org/docs/config/#running-ghost-with-config-env-variables
    App {
        id: "ghost",
        name: "Ghost",
        description: "Publishing platform for blogs, newsletters and memberships",
        category: "CMS",
        docs_url: "https://ghost.org/docs/config/",
        services: &[
            database("mysql"),
            AppService {
                role: "ghost",
                image: "ghost:6.72.0-alpine@sha256:cd617a0901e659cda978b9063f449535d105f5f1a0340ffac415db2c42d22606",
                port: 2368,
                mount_path: Some("/var/lib/ghost/content"),
                domain: true,
                // No health check: with an https `url`, Ghost redirects the
                // plain-HTTP check.
                variables: &[
                    ("url", PUBLIC_URL),
                    ("database__client", "mysql"),
                    ("database__connection__host", "${{mysql.MYSQLHOST}}"),
                    ("database__connection__port", "3306"),
                    ("database__connection__user", "${{mysql.MYSQLUSER}}"),
                    ("database__connection__password", "${{mysql.MYSQLPASSWORD}}"),
                    ("database__connection__database", "${{mysql.MYSQLDATABASE}}"),
                ],
                ..IMAGE
            },
        ],
    },
];

pub fn get(id: &str) -> Option<&'static App> {
    GALLERY.iter().find(|a| a.id == id)
}

impl AppService {
    pub fn database_template(&self) -> Option<&'static Template> {
        self.template
            .map(|t| templates::get(t).expect("app templates name existing database templates"))
    }

    pub fn image(&self) -> &'static str {
        self.database_template().map_or(self.image, |t| t.image)
    }

    pub fn mount_path(&self) -> Option<&'static str> {
        match self.database_template() {
            Some(t) => Some(t.mount_path),
            None => self.mount_path,
        }
    }

    pub fn config(&self) -> ServiceConfig {
        if let Some(t) = self.database_template() {
            return t.config();
        }
        let mut cfg = json!({
            "source": { "type": "image", "image": self.image },
            "port": self.port,
        });
        if let Some(cmd) = self.start_command {
            cfg["startCommand"] = json!(cmd);
        }
        if let Some(path) = self.healthcheck_path {
            cfg["healthcheckPath"] = json!(path);
        }
        serde_json::from_value(cfg).expect("app template configs are valid")
    }

    /// The variables to create, with fresh secrets, and references to the
    /// app's other services rewritten from their role to their name.
    pub fn variables(&self, name_of: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
        if let Some(t) = self.database_template() {
            return t.variables();
        }
        self.variables
            .iter()
            .map(|(k, v)| {
                let v = match *v {
                    SECRET => templates::password(),
                    SECRET_LONG => templates::password() + &templates::password(),
                    v => refs::rename_services(v, &name_of),
                };
                ((*k).to_owned(), v)
            })
            .collect()
    }
}

impl App {
    /// The name each service gets: its role (or `<prefix>-<role>`), with
    /// `-2`, `-3`… when the project already has a service of that name.
    pub fn service_names(&self, prefix: Option<&str>, taken: &[String]) -> Vec<String> {
        let mut used: Vec<String> = taken.iter().map(|n| n.to_lowercase()).collect();
        let mut out = Vec::with_capacity(self.services.len());
        for s in self.services {
            let base = match prefix {
                Some(p) => format!("{p}-{}", s.role),
                None => s.role.to_owned(),
            };
            let name = unique_name(&base, &used);
            used.push(name.clone());
            out.push(name);
        }
        out
    }
}

/// `base`, else `base-2`, `base-3`…, kept within 63 characters.
fn unique_name(base: &str, used: &[String]) -> String {
    let fits = |s: &str, max: usize| -> String {
        s.chars()
            .take(max)
            .collect::<String>()
            .trim_end_matches('-')
            .to_owned()
    };
    let first = fits(base, 63);
    if !used.contains(&first) {
        return first;
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            format!("{}{suffix}", fits(base, 63 - suffix.len()))
        })
        .find(|c| !used.contains(c))
        .expect("an unused name exists")
}

/// What the gallery shows: each app and what deploying it creates.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSummary {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub docs_url: &'static str,
    pub services: Vec<ServiceSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSummary {
    pub role: &'static str,
    pub template: Option<&'static str>,
    pub image: &'static str,
    pub mount_path: Option<&'static str>,
    pub domain: bool,
}

impl App {
    pub fn summary(&self) -> AppSummary {
        AppSummary {
            id: self.id,
            name: self.name,
            description: self.description,
            category: self.category,
            docs_url: self.docs_url,
            services: self
                .services
                .iter()
                .map(|s| ServiceSummary {
                    role: s.role,
                    template: s.template,
                    image: s.image(),
                    mount_path: s.mount_path(),
                    domain: s.domain,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::api::variables::PLATFORM;
    use crate::deploy::refs::{Target, VarKey};

    #[test]
    fn gallery_is_valid() {
        let mut ids = BTreeSet::new();
        for app in GALLERY {
            assert!(ids.insert(app.id), "duplicate app id {}", app.id);
            assert!(app.docs_url.starts_with("https://"), "{}", app.id);
            assert_eq!(
                app.services.iter().filter(|s| s.domain).count(),
                1,
                "{}: exactly one web service",
                app.id
            );
            let roles: Vec<&str> = app.services.iter().map(|s| s.role).collect();
            let unique: BTreeSet<&str> = roles.iter().copied().collect();
            assert_eq!(unique.len(), roles.len(), "{}: duplicate roles", app.id);

            // Every service's variables, by role, before secrets are filled in.
            let declared: BTreeMap<&str, Vec<&str>> = app
                .services
                .iter()
                .map(|s| {
                    let vars = match s.database_template() {
                        Some(t) => t.variables.iter().map(|(k, _)| *k).collect(),
                        None => s.variables.iter().map(|(k, _)| *k).collect(),
                    };
                    (s.role, vars)
                })
                .collect();
            let mut edges: BTreeMap<VarKey, Vec<VarKey>> = BTreeMap::new();
            for s in app.services {
                crate::api::services::validate_service_name(s.role).unwrap();
                let image = s.image();
                if s.template.is_none() {
                    assert!(
                        image.contains("@sha256:"),
                        "{}: {image} is not pinned",
                        s.role
                    );
                    assert!(s.port > 0, "{}", s.role);
                }
                if let Some(p) = s.healthcheck_path {
                    assert!(p.starts_with('/'), "{p}");
                }
                s.config().validate_for_instance().unwrap();
                let raw: &[(&str, &str)] = match s.database_template() {
                    Some(t) => t.variables,
                    None => s.variables,
                };
                for (name, value) in raw {
                    crate::deploy::env::validate_name(name).unwrap();
                    // Plain string replacement of `${{role.` must find every reference.
                    assert!(!value.contains("${{ "), "{}.{name}", s.role);
                    for r in refs::references(value) {
                        let (service, var) = match &r.target {
                            Target::Service(other) => (other.as_str(), r.name.as_str()),
                            Target::Own => (s.role, r.name.as_str()),
                            Target::Shared => panic!("{}.{name}: no shared variables", s.role),
                        };
                        let vars = declared
                            .get(service)
                            .unwrap_or_else(|| panic!("{}.{name}: no service {service}", s.role));
                        assert!(
                            vars.contains(&var) || PLATFORM.contains(&var),
                            "{}.{name}: {service}.{var} does not exist",
                            s.role
                        );
                        if !PLATFORM.contains(&var) {
                            edges
                                .entry((Some(s.role.to_owned()), (*name).to_owned()))
                                .or_default()
                                .push((Some(service.to_owned()), var.to_owned()));
                        }
                    }
                }
            }
            refs::check_cycles(&edges).unwrap();

            // Created variables: secrets filled in, references renamed.
            let names = app.service_names(None, &["postgres".to_owned()]);
            let name_of = |role: &str| {
                app.services
                    .iter()
                    .position(|s| s.role == role)
                    .map(|i| names[i].clone())
            };
            for s in app.services {
                for (k, v) in s.variables(name_of) {
                    crate::deploy::env::validate_value(&v).unwrap();
                    assert!(!v.contains(SECRET) && !v.contains(SECRET_LONG), "{k}");
                    assert!(
                        !v.contains("${{postgres."),
                        "{k}: not renamed to postgres-2"
                    );
                }
            }
        }
    }

    #[test]
    fn secrets_are_fresh_and_long_enough() {
        let plausible = get("plausible").unwrap();
        let web = plausible.services.iter().find(|s| s.domain).unwrap();
        let a = web.variables(|_| None);
        let b = web.variables(|_| None);
        let key = |vars: &[(String, String)]| {
            vars.iter()
                .find(|(k, _)| k == "SECRET_KEY_BASE")
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(key(&a).len(), 64);
        assert_ne!(key(&a), key(&b));
    }

    #[test]
    fn names_are_unique_and_prefixed() {
        let app = get("plausible").unwrap();
        assert_eq!(
            app.service_names(None, &[]),
            ["postgres", "clickhouse", "plausible"]
        );
        let taken = ["postgres".to_owned(), "postgres-2".to_owned()];
        assert_eq!(
            app.service_names(None, &taken),
            ["postgres-3", "clickhouse", "plausible"]
        );
        assert_eq!(
            app.service_names(Some("stats"), &[]),
            ["stats-postgres", "stats-clickhouse", "stats-plausible"]
        );
        let long = "a".repeat(70);
        let used = [long[..63].to_owned()];
        let n = unique_name(&long, &used);
        assert_eq!(n.len(), 63);
        assert!(n.ends_with("-2"));
    }
}
