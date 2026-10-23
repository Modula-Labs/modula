//! The command line: global flags and every command's arguments.

use clap::{Args, Parser, Subcommand, ValueEnum};

const AFTER_HELP: &str = "\
Most commands act on the linked project (`modula-deploy link`), its environment
and its service; -p/-e/-s override them. Read commands take --json for
machine-readable output. Run `modula-deploy docs` for the full reference.

Exit codes: 0 ok, 1 error, 2 usage (including a missing --yes when stdin
isn't a terminal).";

#[derive(Debug, Parser)]
#[command(
    name = "modula-deploy",
    version,
    about = "Modula Deploy from the command line",
    after_help = AFTER_HELP,
    propagate_version = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Command,
}

/// Flags every command accepts.
#[derive(Debug, Clone, Default, Args)]
pub struct Global {
    /// Print machine-readable JSON (results only; hints go to stderr).
    #[arg(long, global = true)]
    pub json: bool,
    /// Project id or name (default: the linked one).
    #[arg(short, long, global = true, env = "MODULA_PROJECT")]
    pub project: Option<String>,
    /// Environment name or id (default: the linked one, else production).
    #[arg(short, long, global = true, env = "MODULA_ENVIRONMENT")]
    pub environment: Option<String>,
    /// Service name or id (default: the linked one, else the only one).
    #[arg(short, long, global = true, env = "MODULA_SERVICE")]
    pub service: Option<String>,
    /// Don't ask for confirmation (required for destructive commands when
    /// stdin isn't a terminal).
    #[arg(short, long, global = true)]
    pub yes: bool,
}

/// `up`: deploy local files without GitHub (honors .gitignore and .modulaignore).
#[derive(Debug, Clone, clap::Args)]
pub struct UpArgs {
    /// Directory to upload (default: the current one).
    #[arg(long, value_name = "DIR")]
    pub path: Option<std::path::PathBuf>,
    /// Don't wait for the deployment to finish.
    #[arg(short, long)]
    pub detach: bool,
    /// A description for the deployment (default: the last commit's subject).
    #[arg(short, long)]
    pub message: Option<String>,
    /// For CI: stream the build logs, never prompt, exit non-zero unless the
    /// deployment goes live (with the runtime logs of a crash).
    #[arg(long, conflicts_with = "detach")]
    pub ci: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Log in with an API token (create one in the dashboard).
    Login {
        /// Your instance, e.g. https://cloud.example.com.
        #[arg(long, env = "MODULA_URL")]
        url: Option<String>,
        /// The token (mdt_…); prompted for when omitted.
        #[arg(long, env = "MODULA_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Save it for this directory only (.modula/token, mode 0600, kept
        /// out of git) instead of your user config.
        #[arg(long)]
        local: bool,
    },
    /// Forget the saved token (this directory's .modula/token if any).
    Logout,
    /// Show who you are logged in as.
    Whoami,
    /// List your projects.
    #[command(visible_alias = "ls")]
    List,
    /// Create a project and link the current directory to it.
    Init {
        /// The project's name.
        name: String,
        /// Workspace id or name (default: your only one).
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Link the current directory to a project (writes .modula/project.toml).
    Link {
        /// Also save this API token for the directory (like `login --local`).
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
    },
    /// Remove the current directory's project link.
    Unlink,
    /// Show the project, environment and each service's latest deployment.
    Status,
    /// Print (and open in a browser) the project's dashboard URL.
    Open,
    /// Pick the default service for this directory, or list services.
    Service {
        /// Service name or id; omitted: list the project's services.
        name: Option<String>,
    },
    /// Create a service from a database template, an image or a repository.
    Add(AddArgs),
    /// Upload the current directory, build it and deploy it to the service.
    Up(UpArgs),
    /// Deploy pending changes: the selected service, or every changed service.
    Deploy {
        /// Every service with pending changes, even with a service linked.
        #[arg(long)]
        all: bool,
        /// Wait until the deployments finish; exit 1 if one fails.
        #[arg(short, long)]
        wait: bool,
    },
    /// Deploy the latest deployment's source again (current variables).
    Redeploy {
        /// Wait until the deployment finishes; exit 1 if it fails.
        #[arg(short, long)]
        wait: bool,
    },
    /// Restart the running deployment's container in place.
    Restart,
    /// Stop and remove the service's latest running deployment.
    Down,
    /// Rebuild and deploy an earlier deployment's Git commit.
    Rollback {
        /// The deployment to go back to (see `deployment list`).
        deployment: String,
        /// Wait until the deployment finishes; exit 1 if it fails.
        #[arg(short, long)]
        wait: bool,
    },
    /// The service's deployments.
    #[command(visible_alias = "deployments")]
    Deployment {
        #[command(subcommand)]
        command: DeploymentCommand,
    },
    /// Show a deployment's logs.
    Logs(LogsArgs),
    /// List, set or delete variables (values are write-only).
    #[command(visible_aliases = ["variable", "vars"])]
    Variables(VariablesArgs),
    /// Switch the linked environment, list environments, or create one.
    #[command(visible_alias = "env")]
    Environment(EnvironmentArgs),
    /// Generate a domain, or add, list, check and delete domains.
    Domain(DomainArgs),
    /// The service's persistent volume.
    Volume {
        #[command(subcommand)]
        command: Option<VolumeCommand>,
    },
    /// Expose a port on the server's public address (for database clients).
    TcpProxy {
        #[command(subcommand)]
        command: Option<TcpProxyCommand>,
    },
    /// Query a Postgres database through the API.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Open a private tunnel to a database and start its client (psql, …).
    Connect {
        /// Service name (default: -s, else the project's only database).
        #[arg(id = "connect_service", value_name = "SERVICE")]
        service: Option<String>,
        /// Only keep a local port open (for GUI clients); don't start a client.
        #[arg(long)]
        tunnel_only: bool,
        /// With --tunnel-only: print the connection string with its password.
        #[arg(long)]
        show_password: bool,
        /// Local port to listen on (default: a free one).
        #[arg(short = 'P', long)]
        port: Option<u16>,
    },
    /// Print a shell completion script.
    Completion {
        /// The shell to complete for.
        shell: clap_complete::Shell,
    },
    /// Print the full command reference (Markdown).
    Docs,
    /// Config as code: plan and apply modula.toml (see `config --help`).
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

/// Where `config plan`/`apply`/`validate` read from.
#[derive(Debug, Clone, Default, Args)]
pub struct ConfigFileArgs {
    /// The config file (default: modula.toml or modula.json here or above).
    #[arg(long, value_name = "PATH")]
    pub file: Option<std::path::PathBuf>,
}

/// What `config plan` and `config apply` compare.
#[derive(Debug, Clone, Default, Args)]
pub struct ConfigPlanArgs {
    #[command(flatten)]
    pub file: ConfigFileArgs,
    /// Every environment (the project's and the file's), not just -e or the
    /// linked one.
    #[arg(long, conflicts_with = "environment")]
    pub all_envs: bool,
    /// Secret values (dotenv) instead of .modula/secrets.<environment>.env.
    #[arg(long, value_name = "PATH")]
    pub secrets_file: Option<std::path::PathBuf>,
    /// Take secret values from environment variables of the same name.
    #[arg(long)]
    pub from_env: bool,
    /// Overwrite sealed variables with the local secret value (sealed values
    /// can't be compared, so they are otherwise only set when missing).
    #[arg(long)]
    pub update_sealed: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Write modula.toml from the project: every environment, never secret
    /// values (sealed and credential-like variables become { secret = true }).
    #[command(visible_alias = "pull")]
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
        /// Where to write (default: modula.toml here).
        #[arg(long, value_name = "PATH")]
        file: Option<std::path::PathBuf>,
    },
    /// Show what `config apply` would change (values of secrets masked).
    Plan(ConfigPlanArgs),
    /// Make the project match the file: services, settings, volumes,
    /// variables, domains. Asks first unless --yes.
    Apply {
        #[command(flatten)]
        plan: ConfigPlanArgs,
        /// Deploy the changed services afterwards.
        #[arg(long)]
        deploy: bool,
    },
    /// Check the file (every environment) without contacting the server.
    Validate(ConfigFileArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Database {
    Postgres,
    Mysql,
    Redis,
    Mongo,
}

impl Database {
    pub fn template(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::Redis => "redis",
            Self::Mongo => "mongo",
        }
    }
}

#[derive(Debug, Args)]
#[group(skip)]
#[command(group(
    clap::ArgGroup::new("source")
        .required(true)
        .args(["database", "image", "repo"])
))]
pub struct AddArgs {
    /// A database from its template.
    #[arg(short, long, value_enum)]
    pub database: Option<Database>,
    /// A prebuilt image, e.g. ghcr.io/acme/api:1.2.
    #[arg(short, long)]
    pub image: Option<String>,
    /// A GitHub repository (owner/name), built on every deploy.
    #[arg(short, long)]
    pub repo: Option<String>,
    /// The repository branch.
    #[arg(short, long, default_value = "main", requires = "repo")]
    pub branch: String,
    /// The service name (default: derived from the database, image or repo).
    #[arg(short, long)]
    pub name: Option<String>,
    /// The port the app listens on (image and repo services; default 8080).
    #[arg(long, conflicts_with = "database")]
    pub port: Option<u16>,
    /// Variables to set in the selected environment (KEY=VALUE, repeatable).
    #[arg(short, long = "variables", value_name = "KEY=VALUE")]
    pub variables: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum DeploymentCommand {
    /// List the service's deployments, newest first.
    #[command(visible_alias = "ls")]
    List {
        /// At most this many (the API keeps the newest 50).
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Deployment id (default: the service's latest).
    #[arg(short, long)]
    pub deployment: Option<String>,
    /// Build logs instead of runtime logs.
    #[arg(short, long)]
    pub build: bool,
    /// Keep streaming new lines until Ctrl-C (or the deployment ends).
    #[arg(short, long)]
    pub follow: bool,
    /// How many recent lines to show (at most 5000).
    #[arg(short = 'n', long, default_value_t = 200)]
    pub lines: u32,
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct VariablesArgs {
    #[command(subcommand)]
    pub command: Option<VariablesCommand>,
    /// The environment's shared variables (${{shared.NAME}}).
    #[arg(long, global = true)]
    pub shared: bool,
    /// Print KEY=VALUE lines (values are masked: the API never returns them).
    #[arg(long)]
    pub kv: bool,
}

#[derive(Debug, Subcommand)]
pub enum VariablesCommand {
    /// Set variables (KEY=VALUE…); they stay pending until deployed.
    Set {
        /// KEY=VALUE pairs (split at the first =).
        #[arg(value_name = "KEY=VALUE", required_unless_present = "stdin")]
        pairs: Vec<String>,
        /// Read KEY=VALUE lines (dotenv style) from stdin, keeping values out
        /// of your shell history.
        #[arg(long)]
        stdin: bool,
        /// Store as sealed: write-only forever, never revealable or copied.
        #[arg(long)]
        sealed: bool,
        /// Deploy the change right away.
        #[arg(long)]
        deploy: bool,
        /// Accepted for Railway compatibility; changes never deploy on
        /// their own here.
        #[arg(long, hide = true)]
        skip_deploys: bool,
    },
    /// Delete variables by name.
    #[command(visible_alias = "rm")]
    Delete {
        /// Variable names.
        #[arg(required = true, value_name = "KEY")]
        keys: Vec<String>,
        /// Deploy the change right away.
        #[arg(long)]
        deploy: bool,
    },
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct EnvironmentArgs {
    #[command(subcommand)]
    pub command: Option<EnvironmentCommand>,
    /// Environment to link this directory to; omitted: list environments.
    pub name: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum EnvironmentCommand {
    /// Create an environment (services are copied; domains are not) and link it.
    New {
        /// The new environment's name.
        name: String,
        /// Copy service settings from this environment (default: the first).
        #[arg(long, value_name = "ENVIRONMENT")]
        copy: Option<String>,
        /// Also copy its variables (sealed ones excepted).
        #[arg(long)]
        copy_variables: bool,
    },
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct DomainArgs {
    #[command(subcommand)]
    pub command: Option<DomainCommand>,
    /// Route the generated domain to this port (default: the service's).
    #[arg(long)]
    pub port: Option<u16>,
}

#[derive(Debug, Subcommand)]
pub enum DomainCommand {
    /// Add a custom domain and print the DNS records to create.
    Add {
        /// The hostname, e.g. api.example.com.
        host: String,
        /// Route it to this port (default: the service's).
        #[arg(long)]
        port: Option<u16>,
    },
    /// List the service's domains.
    #[command(visible_alias = "ls")]
    List,
    /// Look up a custom domain's DNS records now.
    Check {
        /// The hostname.
        host: String,
    },
    /// Remove a domain.
    #[command(visible_alias = "rm")]
    Delete {
        /// The hostname.
        host: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum VolumeCommand {
    /// Attach a volume mounted at PATH (takes effect on the next deploy).
    Add {
        /// Absolute mount path, e.g. /data.
        path: String,
    },
    /// Delete the volume and its data (dashboard only: needs sudo).
    #[command(visible_alias = "rm")]
    Delete,
}

#[derive(Debug, Subcommand)]
pub enum TcpProxyCommand {
    /// Expose the service's port (or --port) on a public port 20000–29999.
    Enable {
        /// The service port to expose (default: the service's port).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Remove the TCP proxy.
    Disable,
}

#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Run SQL: one read-only statement unless --write.
    Query {
        /// The SQL to run.
        sql: String,
        /// Allow writes and several statements (asks for confirmation).
        #[arg(long)]
        write: bool,
    },
    /// List the database's tables.
    Tables,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("modula-deploy").chain(args.iter().copied()))
    }

    #[test]
    fn the_command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_flags_go_anywhere() {
        let cli = parse(&["status", "--json", "-e", "staging", "-s", "api"]).unwrap();
        assert!(cli.global.json);
        assert_eq!(cli.global.environment.as_deref(), Some("staging"));
        assert_eq!(cli.global.service.as_deref(), Some("api"));
        assert!(matches!(cli.command, Command::Status));
        let cli = parse(&["-p", "shop", "--yes", "down"]).unwrap();
        assert_eq!(cli.global.project.as_deref(), Some("shop"));
        assert!(cli.global.yes);
    }

    #[test]
    fn environment_switch_vs_new() {
        let cli = parse(&["environment", "staging"]).unwrap();
        let Command::Environment(e) = cli.command else {
            panic!()
        };
        assert_eq!(e.name.as_deref(), Some("staging"));
        assert!(e.command.is_none());

        let cli = parse(&["env", "new", "preview", "--copy", "staging"]).unwrap();
        let Command::Environment(e) = cli.command else {
            panic!()
        };
        assert!(matches!(
            e.command,
            Some(EnvironmentCommand::New { ref name, ref copy, copy_variables: false })
                if name == "preview" && copy.as_deref() == Some("staging")
        ));
    }

    #[test]
    fn add_needs_exactly_one_source() {
        let cli = parse(&["add", "--database", "postgres"]).unwrap();
        let Command::Add(a) = cli.command else {
            panic!()
        };
        assert_eq!(a.database, Some(Database::Postgres));
        let cli = parse(&[
            "add", "--repo", "acme/api", "--branch", "dev", "--port", "3000",
        ])
        .unwrap();
        let Command::Add(a) = cli.command else {
            panic!()
        };
        assert_eq!(a.branch, "dev");
        assert_eq!(a.port, Some(3000));
        assert!(parse(&["add"]).is_err());
        assert!(parse(&["add", "--image", "nginx", "--repo", "a/b"]).is_err());
        assert!(parse(&["add", "--database", "oracle"]).is_err());
        assert!(parse(&["add", "--database", "redis", "--port", "1"]).is_err());
    }

    #[test]
    fn variables_list_set_and_delete() {
        let cli = parse(&["variables", "--kv"]).unwrap();
        let Command::Variables(v) = cli.command else {
            panic!()
        };
        assert!(v.kv && v.command.is_none());

        let cli = parse(&["vars", "set", "A=1", "B=x=y", "--shared", "--deploy"]).unwrap();
        let Command::Variables(v) = cli.command else {
            panic!()
        };
        assert!(v.shared);
        assert!(matches!(
            v.command,
            Some(VariablesCommand::Set { ref pairs, deploy: true, .. }) if pairs == &["A=1", "B=x=y"]
        ));
        assert!(parse(&["variables", "set"]).is_err());
        assert!(parse(&["variables", "set", "--stdin"]).is_ok());
        assert!(parse(&["variables", "delete"]).is_err());
        // Railway's flag is accepted.
        assert!(parse(&["variables", "set", "A=1", "--skip-deploys"]).is_ok());
    }

    #[test]
    fn domain_generate_vs_subcommands() {
        let cli = parse(&["domain", "--port", "3000"]).unwrap();
        let Command::Domain(d) = cli.command else {
            panic!()
        };
        assert_eq!(d.port, Some(3000));
        assert!(d.command.is_none());
        let cli = parse(&["domain", "add", "api.example.com", "--port", "80"]).unwrap();
        let Command::Domain(d) = cli.command else {
            panic!()
        };
        assert!(matches!(
            d.command,
            Some(DomainCommand::Add { port: Some(80), .. })
        ));
    }

    #[test]
    fn logs_and_db_flags() {
        let cli = parse(&["logs", "--build", "-f", "-n", "50"]).unwrap();
        let Command::Logs(l) = cli.command else {
            panic!()
        };
        assert!(l.build && l.follow);
        assert_eq!(l.lines, 50);
        let cli = parse(&["db", "query", "select 1", "--write"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Db {
                command: DbCommand::Query { write: true, .. }
            }
        ));
        assert!(parse(&["completion", "zsh"]).is_ok());
        // connect's positional service doesn't collide with -s.
        let cli = parse(&["connect", "pg", "-s", "other"]).unwrap();
        assert!(matches!(cli.command, Command::Connect { service: Some(ref s), .. } if s == "pg"));
        assert_eq!(cli.global.service.as_deref(), Some("other"));
        assert!(parse(&["completion", "tcsh"]).is_err());
    }
}
