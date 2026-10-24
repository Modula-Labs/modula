//! Where the CLI keeps state: the login (`~/.config/modula/config.toml`,
//! mode 0600), per-directory project links (`.modula/project.toml`) and an
//! optional per-directory login (`.modula/token`, mode 0600). `.modula/` is
//! kept out of git: see [`ensure_gitignore`].

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use url::Url;

pub const LINK_DIR: &str = ".modula";
pub const LINK_FILE: &str = "project.toml";
/// A login for one directory (`login --local`): the same format as the
/// global config file.
pub const TOKEN_FILE: &str = "token";

/// The saved login.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The instance, e.g. `https://cloud.example.com`.
    pub url: Option<String>,
    /// An API token (`mdt_…`).
    pub token: Option<String>,
    /// Cloudflare Access credentials, for instances in private mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<crate::access::CfAccess>,
}

/// What every API call needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub url: Url,
    pub token: String,
    /// Cloudflare Access credentials (private mode), sent with every request.
    pub access: Option<crate::access::CfAccess>,
}

/// `$XDG_CONFIG_HOME/modula/config.toml`, else `~/.config/…`.
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
    Ok(base.join("modula").join("config.toml"))
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
        // An existing file keeps its mode on open; tighten it either way.
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
        .context("not logged in: run `modula login --url https://your-instance`")?;
    let token = env_token
        .filter(|v| !v.is_empty())
        .or_else(|| cfg.token.clone())
        .context("not logged in: run `modula login` (or set MODULA_TOKEN)")?;
    Ok(Login {
        url: parse_url(&url)?,
        token,
        // A service token in the environment (CI) wins over the saved login.
        access: crate::access::CfAccess::from_process_env().or_else(|| cfg.access.clone()),
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
    /// Default service id (`modula service <name>`).
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
        "# Linked by modula; safe to commit, but usually gitignored.\n{}",
        toml::to_string_pretty(link)?
    );
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

/// The nearest `.modula/token` at or above `start`, and the login in it.
/// A token file that isn't safe to trust (see [`local_token_problem`]) is
/// ignored: the result is then `None`, with a warning saying why.
pub fn find_local_login(start: &Path) -> anyhow::Result<LocalLogin> {
    for dir in start.ancestors() {
        let path = dir.join(LINK_DIR).join(TOKEN_FILE);
        // Not `exists()`: that follows symlinks, and a symlink is refused below.
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        if let Some(problem) = local_token_problem(&path) {
            return Ok(LocalLogin {
                found: None,
                warning: Some(format!(
                    "warning: ignoring {}: it {problem}",
                    path.display()
                )),
            });
        }
        let cfg = load(&path)?;
        return Ok(LocalLogin {
            found: Some((path, cfg)),
            warning: None,
        });
    }
    Ok(LocalLogin::default())
}

/// What [`find_local_login`] found.
#[derive(Debug, Default)]
pub struct LocalLogin {
    /// The token file and the login in it.
    pub found: Option<(PathBuf, Config)>,
    /// Why a token file was ignored.
    pub warning: Option<String>,
}

/// Why a `.modula/token` must not be used, if it mustn't. A checked-out
/// repository (or anyone else on this machine) could otherwise point every
/// command run in it at another instance, with a token of its choosing: the
/// file has to be a regular file (not a symlink), owned by this user,
/// readable by this user only, and not tracked by git.
pub fn local_token_problem(path: &Path) -> Option<String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return Some(format!("cannot be read ({e})")),
    };
    if !meta.file_type().is_file() {
        return Some("is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != current_uid() {
            return Some("belongs to another user".into());
        }
        if meta.mode() & 0o077 != 0 {
            return Some(format!(
                "can be read or written by others (mode {:o}; chmod 600 it)",
                meta.mode() & 0o777
            ));
        }
    }
    if tracked_by_git(path) {
        return Some(
            "is tracked by git (a token must never be committed; git rm --cached it and revoke it)"
                .into(),
        );
    }
    None
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // uid_t is a u32 on every unix Rust supports; geteuid never fails.
    unsafe extern "C" {
        safe fn geteuid() -> u32;
    }
    geteuid()
}

/// `git ls-files --error-unmatch` in the file's directory: true only when
/// git runs and says the file is tracked (no git, or no repository: false).
fn tracked_by_git(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Process-wide: `--allow-insecure-http` was given.
static ALLOW_INSECURE_HTTP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_allow_insecure_http(allow: bool) {
    ALLOW_INSECURE_HTTP.store(allow, std::sync::atomic::Ordering::Relaxed);
}

/// Whether `url` is this machine: localhost, `*.localhost` or a loopback IP.
pub fn is_local(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Before credentials go to `url`: plain http is fine to this machine only,
/// unless `--allow-insecure-http` (or `MODULA_ALLOW_INSECURE_HTTP=1`) says
/// otherwise, and then with a warning.
pub fn check_transport(url: &Url) -> anyhow::Result<()> {
    let env = std::env::var("MODULA_ALLOW_INSECURE_HTTP").ok();
    check_transport_with(
        url,
        ALLOW_INSECURE_HTTP.load(std::sync::atomic::Ordering::Relaxed)
            || matches!(env.as_deref(), Some("1" | "true" | "yes")),
    )
    .map(|warning| {
        static WARNED: std::sync::Once = std::sync::Once::new();
        if let Some(w) = warning {
            WARNED.call_once(|| eprintln!("{w}"));
        }
    })
}

/// [`check_transport`]'s decision: an error, or a warning to print.
fn check_transport_with(url: &Url, allow: bool) -> anyhow::Result<Option<String>> {
    if url.scheme() != "http" || is_local(url) {
        return Ok(None);
    }
    let host = url.host_str().unwrap_or_default();
    if !allow {
        bail!(
            "refusing to send your API token to {host} over plain http; use https://{host}, \
             or pass --allow-insecure-http (MODULA_ALLOW_INSECURE_HTTP=1) if you really mean it"
        );
    }
    Ok(Some(format!(
        "warning: {url} is plain http: your API token travels unencrypted"
    )))
}

/// Write `<dir>/.modula/token` (mode 0600); returns its path.
pub fn save_local_login(dir: &Path, cfg: &Config) -> anyhow::Result<PathBuf> {
    let path = dir.join(LINK_DIR).join(TOKEN_FILE);
    save(&path, cfg)?;
    Ok(path)
}

/// What [`ensure_gitignore`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gitignore {
    /// `.modula/` (or its token and secrets) was already ignored.
    AlreadyIgnored(PathBuf),
    Appended(PathBuf),
    Created(PathBuf),
    /// Not in a git repository and no .gitignore here: nothing to do.
    NoRepository,
}

impl Gitignore {
    /// A line for people, if something changed.
    pub fn describe(&self) -> Option<String> {
        match self {
            Self::Appended(p) => Some(format!(
                "Updated {} to keep .modula/ out of git",
                p.display()
            )),
            Self::Created(p) => Some(format!("Created {} ignoring .modula/", p.display())),
            Self::AlreadyIgnored(_) | Self::NoRepository => None,
        }
    }
}

const GITIGNORE_BLOCK: &str = "\
# Modula Deploy: the directory link, a local API token (.modula/token) and
# secrets (.modula/secrets.*). modula.toml at the repository root is meant to
# be committed.
";

/// Keep `.modula/` (link, local token, secrets files) out of git. The
/// `.gitignore` at the repository root (the nearest directory above `dir`
/// with a `.git`) gets an annotated `.modula/` block unless it already
/// ignores the directory, or names both `.modula/token` and
/// `.modula/secrets.*` (a deliberate choice to commit the link); when it
/// names some `.modula/` paths, only the missing sensitive ones are added.
/// Outside a repository only an existing `<dir>/.gitignore` is updated.
pub fn ensure_gitignore(dir: &Path) -> anyhow::Result<Gitignore> {
    update_gitignore(dir, true)
}

/// Whether [`ensure_gitignore`] would change anything (for read-only
/// commands, which only warn).
pub fn gitignore_pending(dir: &Path) -> bool {
    matches!(
        update_gitignore(dir, false),
        Ok(Gitignore::Appended(_) | Gitignore::Created(_))
    )
}

fn update_gitignore(dir: &Path, write: bool) -> anyhow::Result<Gitignore> {
    let root = dir.ancestors().find(|d| d.join(".git").exists());
    let path = root.unwrap_or(dir).join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    if root.is_none() && existing.is_none() {
        return Ok(Gitignore::NoRepository);
    }
    let text = existing.clone().unwrap_or_default();
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim().trim_start_matches("**/").trim_start_matches('/'))
        .collect();
    let has = |entry: &str| lines.contains(&entry);
    if has(".modula") || has(".modula/") || has(".modula/*") {
        return Ok(Gitignore::AlreadyIgnored(path));
    }
    let sensitive = [".modula/token", ".modula/secrets.*"];
    let missing: Vec<&str> = sensitive.iter().copied().filter(|e| !has(e)).collect();
    if missing.is_empty() {
        return Ok(Gitignore::AlreadyIgnored(path));
    }
    let customized = lines.iter().any(|l| l.starts_with(".modula/"));
    let entries = if customized {
        missing.join("\n")
    } else {
        ".modula/".to_owned()
    };
    let mut new = text;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    if !new.is_empty() {
        new.push('\n');
    }
    new.push_str(GITIGNORE_BLOCK);
    new.push_str(&entries);
    new.push('\n');
    if write {
        std::fs::write(&path, new).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(match existing {
        Some(_) => Gitignore::Appended(path),
        None => Gitignore::Created(path),
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
        let path = dir.join("modula/config.toml");
        assert_eq!(load(&path).unwrap(), Config::default());
        let cfg = Config {
            url: Some("https://cloud.example.com/".into()),
            token: Some("mdt_x".into()),
            ..Default::default()
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
            ..Default::default()
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
    }

    #[test]
    fn gitignore_gets_modula_once() {
        // Outside a repository, without a .gitignore: nothing.
        let dir = temp_dir();
        assert_eq!(ensure_gitignore(&dir).unwrap(), Gitignore::NoRepository);
        assert!(!gitignore_pending(&dir));

        // In a repository: created at its root, from a subdirectory.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let sub = dir.join("apps/api");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(gitignore_pending(&sub));
        assert!(!dir.join(".gitignore").exists(), "checking writes nothing");
        let gi = dir.join(".gitignore");
        assert_eq!(
            ensure_gitignore(&sub).unwrap(),
            Gitignore::Created(gi.clone())
        );
        let text = std::fs::read_to_string(&gi).unwrap();
        assert!(
            text.starts_with("# Modula Deploy") && text.ends_with("\n.modula/\n"),
            "{text}"
        );
        // Idempotent.
        assert_eq!(
            ensure_gitignore(&sub).unwrap(),
            Gitignore::AlreadyIgnored(gi.clone())
        );
        assert_eq!(std::fs::read_to_string(&gi).unwrap(), text);

        // Appended after existing lines (with a missing final newline).
        std::fs::write(&gi, "target/").unwrap();
        assert_eq!(
            ensure_gitignore(&dir).unwrap(),
            Gitignore::Appended(gi.clone())
        );
        let text = std::fs::read_to_string(&gi).unwrap();
        assert!(
            text.starts_with("target/\n\n# Modula") && text.ends_with(".modula/\n"),
            "{text}"
        );

        // A deliberate per-file setup only gets the missing sensitive entry.
        std::fs::write(&gi, "/.modula/token\n").unwrap();
        ensure_gitignore(&dir).unwrap();
        let text = std::fs::read_to_string(&gi).unwrap();
        assert!(
            text.ends_with("\n.modula/secrets.*\n") && !text.contains("\n.modula/\n"),
            "{text}"
        );
        assert_eq!(
            ensure_gitignore(&dir).unwrap(),
            Gitignore::AlreadyIgnored(gi.clone())
        );
        for ignored in ["/.modula/\n", ".modula\n", "**/.modula/\n"] {
            std::fs::write(&gi, ignored).unwrap();
            assert_eq!(
                ensure_gitignore(&dir).unwrap(),
                Gitignore::AlreadyIgnored(gi.clone())
            );
        }
    }

    #[test]
    fn local_logins_are_private_and_found_from_subdirectories() {
        let root = temp_dir();
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(find_local_login(&nested).unwrap().found.is_none());
        let cfg = Config {
            url: Some("https://cloud.example.com/".into()),
            token: Some("mdt_local".into()),
            ..Default::default()
        };
        let path = save_local_login(&root, &cfg).unwrap();
        assert_eq!(path, root.join(".modula/token"));
        let local = find_local_login(&nested).unwrap();
        assert_eq!(local.found, Some((path.clone(), cfg)));
        assert!(local.warning.is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    fn local_token(root: &Path) -> PathBuf {
        let cfg = Config {
            url: Some("https://evil.example.com/".into()),
            token: Some("mdt_local".into()),
            ..Default::default()
        };
        save_local_login(root, &cfg).unwrap()
    }

    fn ignored_because(root: &Path) -> String {
        let local = find_local_login(root).unwrap();
        assert!(local.found.is_none(), "the token file was used");
        local.warning.expect("a warning")
    }

    #[cfg(unix)]
    #[test]
    fn local_tokens_readable_by_others_are_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir();
        let path = local_token(&root);
        for mode in [0o640, 0o604, 0o620, 0o644] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let w = ignored_because(&root);
            assert!(w.contains("read or written by others"), "{mode:o}: {w}");
            assert!(w.contains(&path.display().to_string()), "{w}");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(find_local_login(&root).unwrap().found.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn local_tokens_must_be_regular_files() {
        let root = temp_dir();
        let elsewhere = temp_dir();
        let target = local_token(&elsewhere);
        std::fs::create_dir_all(root.join(LINK_DIR)).unwrap();
        std::os::unix::fs::symlink(&target, root.join(LINK_DIR).join(TOKEN_FILE)).unwrap();
        assert!(ignored_because(&root).contains("not a regular file"));

        // A directory named token: also not used, and no ancestor's instead.
        let root = temp_dir();
        local_token(&root);
        let nested = root.join("sub");
        std::fs::create_dir_all(nested.join(LINK_DIR).join(TOKEN_FILE)).unwrap();
        assert!(ignored_because(&nested).contains("not a regular file"));
    }

    #[test]
    fn local_tokens_tracked_by_git_are_ignored() {
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let root = temp_dir();
        if !git(&root, &["init", "-q"]) {
            eprintln!("git isn't available; skipping");
            return;
        }
        let path = local_token(&root);
        // In a repository but untracked: used.
        assert!(find_local_login(&root).unwrap().found.is_some());
        assert!(git(&root, &["add", "-f", ".modula/token"]));
        let w = ignored_because(&root);
        assert!(w.contains("tracked by git"), "{w}");
        assert!(local_token_problem(&path).is_some());
    }

    #[test]
    fn plain_http_only_to_this_machine() {
        for local in [
            "http://localhost:8080",
            "http://127.0.0.1",
            "http://[::1]:3000",
            "http://app.localhost",
            "http://LOCALHOST",
        ] {
            let url = parse_url(local).unwrap();
            assert!(is_local(&url), "{local}");
            assert_eq!(check_transport_with(&url, false).unwrap(), None, "{local}");
        }
        let https = parse_url("https://cloud.example.com").unwrap();
        assert_eq!(check_transport_with(&https, false).unwrap(), None);
        for remote in [
            "http://cloud.example.com",
            "http://10.0.0.5",
            "http://localhost.example.com",
        ] {
            let url = parse_url(remote).unwrap();
            assert!(!is_local(&url), "{remote}");
            let e = check_transport_with(&url, false).unwrap_err().to_string();
            assert!(e.contains("--allow-insecure-http"), "{e}");
            let w = check_transport_with(&url, true)
                .unwrap()
                .expect("a warning");
            assert!(w.contains("plain http"), "{w}");
        }
    }
}
