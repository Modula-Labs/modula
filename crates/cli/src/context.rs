//! What every command runs with: the login, the directory's link, the global
//! flags and where output goes. Resolves `-p/-e/-s` and the link into the
//! project, environment and service instance to act on.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context as _;

use crate::api::{Api, Environment, ProjectDetail, Service};
use crate::cli::Global;
use crate::config::{self, Config, Link, SavedLogin};
use crate::output::{Output, usage};
use crate::resolve;

/// Inputs from the process (or a test).
pub struct Session {
    pub config_path: PathBuf,
    pub cwd: PathBuf,
    /// `MODULA_URL` / `MODULA_TOKEN`.
    pub env_url: Option<String>,
    pub env_token: Option<String>,
    /// Whether we may prompt (stdin is a terminal).
    pub interactive: bool,
}

impl Session {
    pub fn from_process() -> anyhow::Result<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Ok(Self {
            config_path: config::config_path()?,
            cwd: std::env::current_dir().context("cannot read the current directory")?,
            env_url: var("MODULA_URL"),
            env_token: var("MODULA_TOKEN"),
            interactive: std::io::stdin().is_terminal(),
        })
    }
}

pub struct Ctx {
    pub session: Session,
    pub global: Global,
    pub out: Output,
    pub cfg: Config,
    /// The nearest `.modula/project.toml` and its path.
    pub link: Option<(PathBuf, Link)>,
    /// The nearest `.modula/token` (`login --local`) and its login.
    pub local_login: Option<(PathBuf, SavedLogin)>,
    api: OnceLock<Api>,
}

/// The saved login a command uses, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveLogin {
    /// The local name, for a login from the user config.
    pub name: Option<String>,
    /// The `.modula/token` it came from, for a directory login.
    pub file: Option<PathBuf>,
    pub saved: SavedLogin,
}

/// One service in one environment of a project.
pub struct Target {
    pub project: ProjectDetail,
    pub environment: Environment,
    pub service: Service,
    pub instance: String,
}

impl Target {
    /// `api (shop/production)`.
    pub fn label(&self) -> String {
        format!(
            "{} ({}/{})",
            self.service.name, self.project.name, self.environment.name
        )
    }
}

impl Ctx {
    pub fn new(session: Session, global: Global, out: Output) -> anyhow::Result<Self> {
        let cfg = config::load(&session.config_path)?;
        let link = config::find_link(&session.cwd)?;
        let local = config::find_local_login(&session.cwd)?;
        if let Some(w) = &local.warning {
            out.note(w);
        }
        let local_login = local.found;
        Ok(Self {
            session,
            global,
            out,
            cfg,
            link,
            local_login,
            api: OnceLock::new(),
        })
    }

    /// The saved login in effect: `--login` / `MODULA_LOGIN` if given, else
    /// this directory's `.modula/token`, else the login the directory was
    /// linked with, else the default one.
    pub fn active_login(&self) -> anyhow::Result<Option<ActiveLogin>> {
        let explicit = self.global.login.as_deref();
        if explicit.is_none()
            && let Some((file, saved)) = &self.local_login
        {
            return Ok(Some(ActiveLogin {
                name: None,
                file: Some(file.clone()),
                saved: saved.clone(),
            }));
        }
        let linked = self.linked().and_then(|l| l.login.as_deref());
        Ok(self
            .cfg
            .select(explicit, linked)?
            .map(|(name, saved)| ActiveLogin {
                name: Some(name.to_owned()),
                file: None,
                saved: saved.clone(),
            }))
    }

    /// The API client; fails when not logged in. The token comes from
    /// `MODULA_TOKEN`, else the [active login](Self::active_login).
    pub fn api(&self) -> anyhow::Result<&Api> {
        if let Some(api) = self.api.get() {
            return Ok(api);
        }
        // With a token in the environment (CI), a saved login only supplies
        // the URL, so a missing one isn't an error unless named with --login.
        let active = if self.session.env_token.is_some() && self.global.login.is_none() {
            self.active_login().ok().flatten()
        } else {
            self.active_login()?
        };
        let login = config::login(
            active.as_ref().map(|a| &a.saved),
            self.session.env_url.clone(),
            self.session.env_token.clone(),
        )?;
        if let Some(note) = self.local_instance_notice(&login) {
            self.out.note(note);
        }
        let api = Api::new(login)?;
        Ok(self.api.get_or_init(|| api))
    }

    /// A line naming the instance when this directory's `.modula/token`
    /// sends commands somewhere other than the default login does.
    fn local_instance_notice(&self, login: &config::Login) -> Option<String> {
        if self.session.env_url.is_some() || self.global.login.is_some() {
            return None;
        }
        let (path, local) = self.local_login.as_ref()?;
        let local_url = config::parse_url(&local.url).ok()?;
        let default_url = self
            .cfg
            .default_name()
            .and_then(|n| config::parse_url(&self.cfg.logins[n].url).ok());
        (local_url == login.url && default_url.as_ref() != Some(&local_url))
            .then(|| format!("using instance {local_url} from {}", path.display()))
    }

    pub fn linked(&self) -> Option<&Link> {
        self.link.as_ref().map(|(_, l)| l)
    }

    /// The link, if it is for `project` (a `-p` elsewhere ignores it).
    fn link_for(&self, project: &str) -> Option<&Link> {
        self.linked().filter(|l| l.project == project)
    }

    pub async fn project(&self) -> anyhow::Result<ProjectDetail> {
        let wanted = self
            .global
            .project
            .as_deref()
            .or(self.linked().map(|l| l.project.as_str()));
        resolve::project(self.api()?, wanted).await
    }

    pub fn environment<'a>(&self, p: &'a ProjectDetail) -> anyhow::Result<&'a Environment> {
        let wanted = self
            .global
            .environment
            .as_deref()
            .or(self.link_for(&p.id).and_then(|l| l.environment.as_deref()));
        resolve::pick_environment(&p.environments, wanted)
    }

    /// The service named on the command line, else the linked one.
    pub fn wanted_service<'a>(&'a self, p: &ProjectDetail) -> Option<&'a str> {
        self.global
            .service
            .as_deref()
            .or(self.link_for(&p.id).and_then(|l| l.service.as_deref()))
    }

    pub fn service<'a>(&self, p: &'a ProjectDetail) -> anyhow::Result<&'a Service> {
        resolve::default_service(&p.services, self.wanted_service(p))
    }

    /// For database commands: `-s`, else the linked service when it is a
    /// database, else the project's only database.
    pub fn database_service<'a>(&self, p: &'a ProjectDetail) -> anyhow::Result<&'a Service> {
        let linked_db = self
            .link_for(&p.id)
            .and_then(|l| l.service.as_deref())
            .filter(|id| {
                p.services
                    .iter()
                    .any(|s| s.id == *id && s.template.is_some())
            });
        resolve::pick_service(&p.services, self.global.service.as_deref().or(linked_db))
    }

    /// The project, environment and service to act on.
    pub async fn target(&self) -> anyhow::Result<Target> {
        let project = self.project().await?;
        let service = self.service(&project)?.clone();
        self.target_with(project, service)
    }

    pub async fn database_target(&self) -> anyhow::Result<Target> {
        let project = self.project().await?;
        let service = self.database_service(&project)?.clone();
        self.target_with(project, service)
    }

    pub fn target_with(&self, project: ProjectDetail, service: Service) -> anyhow::Result<Target> {
        let environment = self.environment(&project)?.clone();
        let instance = resolve::instance_id(&project, &service, &environment)?;
        Ok(Target {
            project,
            environment,
            service,
            instance,
        })
    }

    /// Write the link in the linked directory (or here when there is none).
    /// A new link records the named login in use (not one from the
    /// environment or `.modula/token`), so commands here keep using it.
    pub fn save_link(&mut self, mut link: Link) -> anyhow::Result<PathBuf> {
        if link.login.is_none() && self.session.env_token.is_none() {
            link.login = self.active_login().ok().flatten().and_then(|a| a.name);
        }
        let dir = match &self.link {
            Some((file, _)) => file
                .parent()
                .and_then(|d| d.parent())
                .map(PathBuf::from)
                .unwrap_or_else(|| self.session.cwd.clone()),
            None => self.session.cwd.clone(),
        };
        let file = config::write_link(&dir, &link)?;
        self.link = Some((file.clone(), link));
        self.ignore_modula_dir(&dir);
        Ok(file)
    }

    /// The directory a `.modula/` written now belongs in: the linked one,
    /// else the current one.
    pub fn modula_root(&self) -> PathBuf {
        self.link
            .as_ref()
            .and_then(|(file, _)| file.parent().and_then(|d| d.parent()))
            .map(PathBuf::from)
            .unwrap_or_else(|| self.session.cwd.clone())
    }

    /// After writing into `<dir>/.modula/`: keep it out of git (best effort;
    /// says what changed on stderr).
    pub fn ignore_modula_dir(&self, dir: &std::path::Path) {
        match config::ensure_gitignore(dir) {
            Ok(g) => {
                if let Some(line) = g.describe() {
                    self.out.note(line);
                }
            }
            Err(e) => self
                .out
                .note(format!("Warning: add .modula/ to your .gitignore ({e:#})")),
        }
    }

    /// Ask before something destructive. `--yes` skips the question; without
    /// a terminal to ask on, `--yes` is required (exit code 2).
    pub fn confirm(&self, question: &str) -> anyhow::Result<()> {
        if self.global.yes {
            return Ok(());
        }
        if !self.session.interactive {
            return Err(usage(format!("{question} Pass --yes to confirm.")));
        }
        eprint!("{question} [y/N] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut answer)
            .context("cannot read the answer")?;
        if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
            Ok(())
        } else {
            anyhow::bail!("cancelled")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("modula-ctx-test-{n}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("app/src")).unwrap();
        dir
    }

    fn saved(url: &str, token: &str) -> SavedLogin {
        SavedLogin {
            url: url.into(),
            token: token.into(),
            access: None,
        }
    }

    /// Two logins, `home` the default.
    fn two_logins(dir: &std::path::Path) {
        let mut cfg = Config::default();
        cfg.set("work", saved("https://work.example.com", "mdt_work"));
        cfg.set("home", saved("https://home.example.com", "mdt_home"));
        config::save(&dir.join("global.toml"), &cfg).unwrap();
    }

    fn ctx(dir: &std::path::Path, env_token: Option<&str>, login: Option<&str>) -> Ctx {
        let session = Session {
            config_path: dir.join("global.toml"),
            cwd: dir.join("app/src"),
            env_url: None,
            env_token: env_token.map(Into::into),
            interactive: false,
        };
        let global = Global {
            login: login.map(Into::into),
            ..Global::default()
        };
        Ctx::new(session, global, Output::capture(false).0).unwrap()
    }

    fn token(dir: &std::path::Path, env_token: Option<&str>, login: Option<&str>) -> String {
        ctx(dir, env_token, login)
            .api()
            .unwrap()
            .login
            .token
            .clone()
    }

    fn link(dir: &std::path::Path, login: Option<&str>) {
        let mut l = Link::new(
            "0123456789abcdef",
            "shop",
            &crate::api::Environment {
                id: "1123456789abcdef".into(),
                name: "production".into(),
                is_default: true,
            },
        );
        l.login = login.map(Into::into);
        config::write_link(&dir.join("app"), &l).unwrap();
    }

    #[test]
    fn logins_resolve_env_flag_directory_link_then_default() {
        let dir = temp_dir();
        two_logins(&dir);
        assert_eq!(token(&dir, None, None), "mdt_home");
        assert_eq!(token(&dir, None, Some("work")), "mdt_work");

        // A linked directory uses its login; --login still wins.
        link(&dir, Some("work"));
        assert_eq!(token(&dir, None, None), "mdt_work");
        assert_eq!(token(&dir, None, Some("home")), "mdt_home");

        // A directory token wins over the link (found from below)...
        config::save_local_login(
            &dir.join("app"),
            &saved("https://work.example.com", "mdt_local"),
        )
        .unwrap();
        assert_eq!(token(&dir, None, None), "mdt_local");
        // ...but not over --login, and MODULA_TOKEN wins over everything.
        assert_eq!(token(&dir, None, Some("home")), "mdt_home");
        assert_eq!(token(&dir, Some("mdt_env"), None), "mdt_env");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_login_is_a_clear_error() {
        let dir = temp_dir();
        two_logins(&dir);
        let e = ctx(&dir, None, Some("nope"))
            .api()
            .err()
            .unwrap()
            .to_string();
        assert!(
            e.contains("no login named nope (logins: home, work)"),
            "{e}"
        );
        link(&dir, Some("gone"));
        let e = ctx(&dir, None, None).api().err().unwrap().to_string();
        assert!(e.contains("linked with the login gone"), "{e}");
        // CI: a token in the environment doesn't need the saved login, but
        // still needs a URL.
        let e = ctx(&dir, Some("mdt_env"), None)
            .api()
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("MODULA_URL"), "{e}");
        // Nothing saved at all.
        let empty = temp_dir();
        let e = ctx(&empty, None, None).api().err().unwrap().to_string();
        assert!(e.contains("not logged in"), "{e}");
    }

    #[test]
    fn new_links_record_the_login_in_use() {
        let dir = temp_dir();
        two_logins(&dir);
        let env = crate::api::Environment {
            id: "1123456789abcdef".into(),
            name: "production".into(),
            is_default: true,
        };
        let mut c = ctx(&dir, None, Some("work"));
        c.session.cwd = dir.join("app");
        let file = c
            .save_link(Link::new("0123456789abcdef", "shop", &env))
            .unwrap();
        let (_, l) = config::find_link(&dir.join("app")).unwrap().unwrap();
        assert_eq!(l.login.as_deref(), Some("work"), "{}", file.display());
        // With a token from the environment, nothing is recorded.
        let mut c = ctx(&dir, Some("mdt_env"), None);
        c.session.cwd = dir.join("app/src");
        c.link = None;
        c.save_link(Link::new("0123456789abcdef", "shop", &env))
            .unwrap();
        let (_, l) = config::find_link(&dir.join("app/src")).unwrap().unwrap();
        assert_eq!(l.login, None);
    }

    #[test]
    fn a_directory_login_elsewhere_is_announced() {
        let dir = temp_dir();
        let mut global = Config::default();
        global.set("g", saved("https://global.example.com", "mdt_global"));
        config::save(&dir.join("global.toml"), &global).unwrap();
        let notice = |local_url: &str| {
            config::save_local_login(&dir.join("app"), &saved(local_url, "mdt_local")).unwrap();
            let ctx = ctx(&dir, None, None);
            let active = ctx.active_login().unwrap().unwrap();
            let login = config::login(Some(&active.saved), None, None).unwrap();
            ctx.local_instance_notice(&login)
        };
        let n = notice("https://other.example.com").expect("a notice");
        assert!(
            n.starts_with("using instance https://other.example.com/ from ")
                && n.ends_with(".modula/token"),
            "{n}"
        );
        assert_eq!(notice("https://global.example.com/"), None);
    }
}
