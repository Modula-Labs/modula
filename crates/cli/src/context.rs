//! What every command runs with: the login, the directory's link, the global
//! flags and where output goes. Resolves `-p/-e/-s` and the link into the
//! project, environment and service instance to act on.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context as _;

use crate::api::{Api, Environment, ProjectDetail, Service};
use crate::cli::Global;
use crate::config::{self, Config, Link};
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
    api: OnceLock<Api>,
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
        Ok(Self {
            session,
            global,
            out,
            cfg,
            link,
            api: OnceLock::new(),
        })
    }

    /// The API client; fails when not logged in.
    pub fn api(&self) -> anyhow::Result<&Api> {
        if let Some(api) = self.api.get() {
            return Ok(api);
        }
        let login = config::login(
            &self.cfg,
            self.session.env_url.clone(),
            self.session.env_token.clone(),
        )?;
        let api = Api::new(login)?;
        Ok(self.api.get_or_init(|| api))
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

    fn target_with(&self, project: ProjectDetail, service: Service) -> anyhow::Result<Target> {
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
    pub fn save_link(&mut self, link: Link) -> anyhow::Result<PathBuf> {
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
        Ok(file)
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
