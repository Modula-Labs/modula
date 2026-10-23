//! Where the CLI keeps state: the login (`~/.config/modula-deploy/config.toml`,
//! mode 0600) and per-directory project links (`.modula/project.toml`).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use url::Url;

pub const LINK_DIR: &str = ".modula";
pub const LINK_FILE: &str = "project.toml";

/// The saved login.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The instance, e.g. `https://cloud.example.com`.
    pub url: Option<String>,
    /// An API token (`mdt_…`).
    pub token: Option<String>,
}

/// What every API call needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub url: Url,
    pub token: String,
}

/// `$XDG_CONFIG_HOME/modula-deploy/config.toml`, else `~/.config/…`.
pub fn config_path() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .context("cannot find your home directory (set HOME)")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("modula-deploy").join("config.toml"))
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("{} is invalid", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Write the config readable by the owner only (it holds a token).
pub fn save(path: &Path, cfg: &Config) -> anyhow::Result<()> {
    let dir = path.parent().context("config path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = path.with_extension("toml.tmp");
    write_private(&tmp, toml::to_string_pretty(cfg)?.as_bytes())?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(0o600);
        // An older file keeps its mode on open; tighten it either way.
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Normalize an instance URL: https by default, no path.
pub fn parse_url(raw: &str) -> anyhow::Result<Url> {
    let raw = raw.trim().trim_end_matches('/');
    let with_scheme = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    let mut url = Url::parse(&with_scheme).with_context(|| format!("invalid URL {raw}"))?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        bail!("the URL must be http(s)://host");
    }
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// The effective login: `MODULA_URL` / `MODULA_TOKEN` override the file.
pub fn login(
    cfg: &Config,
    env_url: Option<String>,
    env_token: Option<String>,
) -> anyhow::Result<Login> {
    let url = env_url
        .filter(|v| !v.is_empty())
        .or_else(|| cfg.url.clone())
        .context("not logged in: run `modula-deploy login --url https://your-instance`")?;
    let token = env_token
        .filter(|v| !v.is_empty())
        .or_else(|| cfg.token.clone())
        .context("not logged in: run `modula-deploy login` (or set MODULA_TOKEN)")?;
    Ok(Login {
        url: parse_url(&url)?,
        token,
    })
}

/// A directory linked to a project (and optionally an environment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Project id.
    pub project: String,
    /// For humans reading the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Default service id (`modula-deploy service <name>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
}

impl Link {
    pub fn new(project: &str, project_name: &str, environment: &str) -> Self {
        Self {
            project: project.to_owned(),
            project_name: Some(project_name.to_owned()),
            environment: Some(environment.to_owned()),
            service: None,
            service_name: None,
        }
    }
}

/// The nearest `.modula/project.toml` at or above `start`.
pub fn find_link(start: &Path) -> anyhow::Result<Option<(PathBuf, Link)>> {
    for dir in start.ancestors() {
        let path = dir.join(LINK_DIR).join(LINK_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let link = toml::from_str(&text)
                    .with_context(|| format!("{} is invalid", path.display()))?;
                return Ok(Some((path, link)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }
    Ok(None)
}

/// Write `<dir>/.modula/project.toml`; returns its path.
pub fn write_link(dir: &Path, link: &Link) -> anyhow::Result<PathBuf> {
    let link_dir = dir.join(LINK_DIR);
    std::fs::create_dir_all(&link_dir)
        .with_context(|| format!("cannot create {}", link_dir.display()))?;
    let path = link_dir.join(LINK_FILE);
    let text = format!(
        "# Linked by modula-deploy; safe to commit, but usually gitignored.\n{}",
        toml::to_string_pretty(link)?
    );
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

/// Whether `<dir>/.gitignore` exists and doesn't mention `.modula`.
pub fn gitignore_lacks_link(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(".gitignore")).is_ok_and(|g| {
        !g.lines()
            .map(str::trim)
            .any(|l| l.trim_start_matches('/').trim_end_matches('/') == LINK_DIR)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let n: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let dir = std::env::temp_dir().join(format!("modula-cli-test-{n}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_round_trips_privately() {
        let dir = temp_dir();
        let path = dir.join("modula-deploy/config.toml");
        assert_eq!(load(&path).unwrap(), Config::default());
        let cfg = Config {
            url: Some("https://cloud.example.com/".into()),
            token: Some("mdt_x".into()),
        };
        save(&path, &cfg).unwrap();
        assert_eq!(load(&path).unwrap(), cfg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn environment_overrides_the_file() {
        let cfg = Config {
            url: Some("cloud.example.com".into()),
            token: Some("mdt_file".into()),
        };
        let l = login(&cfg, None, None).unwrap();
        assert_eq!(l.url.as_str(), "https://cloud.example.com/");
        assert_eq!(l.token, "mdt_file");
        let l = login(
            &cfg,
            Some("http://localhost:8080/app".into()),
            Some("mdt_env".into()),
        )
        .unwrap();
        assert_eq!(l.url.as_str(), "http://localhost:8080/");
        assert_eq!(l.token, "mdt_env");
        // Empty variables don't count.
        assert_eq!(
            login(&cfg, Some(String::new()), Some(String::new()))
                .unwrap()
                .token,
            "mdt_file"
        );
        assert!(login(&Config::default(), None, None).is_err());
        assert!(parse_url("ftp://x").is_err());
    }

    #[test]
    fn links_are_found_from_subdirectories() {
        let root = temp_dir();
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let link = Link {
            project: "0123456789abcdef".into(),
            project_name: Some("shop".into()),
            environment: Some("staging".into()),
            service: Some("1123456789abcdef".into()),
            service_name: Some("api".into()),
        };
        let path = write_link(&root, &link).unwrap();
        let (found, got) = find_link(&nested).unwrap().unwrap();
        assert_eq!(found, path);
        assert_eq!(got, link);

        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        assert!(gitignore_lacks_link(&root));
        std::fs::write(root.join(".gitignore"), "target/\n/.modula/\n").unwrap();
        assert!(!gitignore_lacks_link(&root));
    }
}
