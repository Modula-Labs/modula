//! The command line: global flags and every command's arguments.

use clap::{Args, Parser, Subcommand, ValueEnum};

const AFTER_HELP: &str = "\
Most commands act on the linked project (`modula link`), its environment
and its service; -p/-e/-s override them. Read commands take --json for
machine-readable output. Run `modula docs` for the full reference.

Exit codes: 0 ok, 1 error, 2 usage (including a missing --yes when stdin
isn't a terminal).";

#[derive(Debug, Parser)]
#[command(
    name = "modula",
    version,
    about = "Ship your apps to your own servers with Modula Deploy.",
    long_about = "Ship your apps to your own servers with Modula Deploy.\n\n\
New here? Start with `modula login --url https://your-instance`, then\n\
`modula init <name>` and `modula up` to deploy the current directory.",
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
    /// Allow sending the API token over plain http to an instance that isn't
    /// this machine (it travels unencrypted).
    #[arg(long, global = true, env = "MODULA_ALLOW_INSECURE_HTTP")]
    pub allow_insecure_http: bool,
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
    /// Set up this directory: create a project (or pick one with -p) and
    /// link to it, then optionally write .modula/modula.ts from it. Without
    /// a terminal: pass a name or -p (and --format to write the config).
    Init {
        /// A new project's name (in a terminal you can pick from a list).
        name: Option<String>,
        /// Workspace id or name (default: your only one).
        #[arg(short, long)]
        workspace: Option<String>,
        /// Write the project's config to .modula/ in this format.
        #[arg(long, value_enum)]
        format: Option<ConfigFormat>,
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
    /// Create a service from a database template, an image, a repository or
    /// a JavaScript function file, or an app and its databases from a template.
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
    /// The service's deployments (alone: list them).
    #[command(visible_alias = "deployments")]
    Deployment {
        #[command(subcommand)]
        command: Option<DeploymentCommand>,
    },
    /// Show a deployment's logs.
    Logs(LogsArgs),
    /// Show or change where the service's code comes from: connect a GitHub
    /// repository (or another one), an image, or local files (`--local`).
    Source(SourceArgs),
    /// Show the service's CPU, memory and network usage.
    Metrics {
        /// How far back: 1h, 6h, 24h or 7d.
        #[arg(long, default_value = "1h", value_parser = ["1h", "6h", "24h", "7d"])]
        range: String,
    },
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
    /// Open a shell in the service's running container, or run a command
    /// there (`modula ssh api -- ls -la`).
    Ssh {
        /// Service name (default: -s, else the linked one).
        #[arg(id = "ssh_service", value_name = "SERVICE")]
        service: Option<String>,
        /// Which replica's container (default: the first running one).
        #[arg(long)]
        replica: Option<u16>,
        /// The command (default: a login shell, bash when the image has it).
        #[arg(last = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Run a command here with the service's variables injected
    /// (`modula run -- npm run migrate`). They are only held in memory;
    /// sealed variables are never sent.
    Run {
        /// The command and its arguments.
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },
    /// Print a shell completion script.
    Completion {
        /// The shell to complete for.
        shell: clap_complete::Shell,
    },
    /// Print the full command reference (Markdown).
    Docs,
    /// Print the guide for people and AI agents: install, create a server,
    /// log in, deploy, non-interactive use, troubleshooting (Markdown).
    Guide,
    /// Create a new Modula server at a cloud provider (Hetzner), or list
    /// what can be created.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// A workspace's members and invite links.
    #[command(visible_alias = "member")]
    Members {
        /// Workspace id or name (default: your only one).
        #[arg(short, long, global = true)]
        workspace: Option<String>,
        #[command(subcommand)]
        command: Option<MembersCommand>,
    },
    /// A cron service's schedule and runs; run it now.
    Cron {
        #[command(subcommand)]
        command: CronCommand,
    },
    /// This Modula instance: its dashboard domain and apps domain.
    Instance {
        #[command(subcommand)]
        command: InstanceCommand,
    },
    /// Config as code: plan and apply .modula/modula.ts (or .toml/.json).
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

/// The language of a written config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ConfigFormat {
    /// .modula/modula.ts (runs with Node 22.18+ or Bun).
    Ts,
    /// .modula/modula.toml.
    Toml,
}

/// Where `config plan`/`apply`/`validate` read from.
#[derive(Debug, Clone, Default, Args)]
pub struct ConfigFileArgs {
    /// The config file (default: .modula/modula.ts, .modula/modula.toml or
    /// .modula/modula.json here or above).
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
pub enum MembersCommand {
    /// List members with their role and whether 2FA is on (the default).
    #[command(visible_alias = "ls")]
    List,
    /// Create a single-use invite link and print it (shown once; no email
    /// is sent, share it yourself).
    Invite {
        /// The role the link grants.
        #[arg(long, default_value = "member", value_parser = ["admin", "member", "deployer", "viewer"])]
        role: String,
        /// Days until the link expires (1-30, default 7).
        #[arg(long, value_name = "DAYS")]
        expires_in_days: Option<i64>,
    },
    /// Remove a member by email. Needs a re-authenticated dashboard session,
    /// so with an API token this points you to the dashboard.
    Remove {
        /// The member's email.
        email: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum CronCommand {
    /// The schedule, its next runs and the latest runs.
    #[command(visible_alias = "ls")]
    List {
        /// Service name or id (default: the selected service).
        service: Option<String>,
    },
    /// Run the service once now, from its serving deployment.
    Run {
        /// Service name or id (default: the selected service).
        service: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum InstanceCommand {
    /// Show or set the dashboard's domain and the apps domain (a server
    /// installs without one and is reached at its IP until then).
    Domain {
        #[command(subcommand)]
        command: Option<InstanceDomainCommand>,
    },
}

#[derive(Debug, Subcommand)]
pub enum InstanceDomainCommand {
    /// Set the domains. DNS must point at the server first (A records for the
    /// dashboard domain and *.<apps domain>); flags left out keep their value.
    Set {
        /// The dashboard's domain, e.g. deploy.example.com.
        #[arg(long)]
        dashboard: Option<String>,
        /// Parent of generated app domains, e.g. apps.example.com.
        #[arg(long)]
        apps_domain: Option<String>,
        /// Save even if DNS doesn't answer with this server (e.g. a proxied
        /// Cloudflare record).
        #[arg(long)]
        skip_dns_check: bool,
    },
    /// Remove the dashboard's domain (back to the IP address).
    Clear,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Write .modula/modula.ts (or .toml) from the project: every
    /// environment, never secret values (sealed and credential-like
    /// variables become secrets).
    #[command(visible_alias = "pull")]
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
        /// Where to write (default: .modula/modula.<format> in the linked
        /// directory; the format follows the extension).
        #[arg(long, value_name = "PATH")]
        file: Option<std::path::PathBuf>,
        /// The file's language (default: ts).
        #[arg(long, value_enum)]
        format: Option<ConfigFormat>,
        /// Only these services, as a partial file (repeatable; the
        /// project's other services are left alone).
        #[arg(long = "only", value_name = "SERVICE")]
        only: Vec<String>,
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

/// What `add --function` writes functions for (the newest the servers'
/// workerd supports is the server's `WORKERD_COMPATIBILITY_DATE`).
pub const DEFAULT_COMPATIBILITY_DATE: &str = "2026-10-01";

#[derive(Debug, Args)]
#[group(skip)]
#[command(group(
    clap::ArgGroup::new("source")
        .required(true)
        .args(["database", "image", "repo", "template", "function"])
))]
pub struct AddArgs {
    /// A database from its template.
    #[arg(short, long, value_enum)]
    pub database: Option<Database>,
    /// A one-click app with the databases it needs (n8n, plausible, umami,
    /// metabase, uptime-kuma, ghost; `--template list` lists them), deployed
    /// right away. `--name` prefixes its service names.
    #[arg(short, long, conflicts_with_all = ["port", "variables"])]
    pub template: Option<String>,
    /// A prebuilt image, e.g. ghcr.io/acme/api:1.2.
    #[arg(short, long)]
    pub image: Option<String>,
    /// A GitHub repository (owner/name), built on every deploy.
    #[arg(short, long)]
    pub repo: Option<String>,
    /// The repository branch.
    #[arg(short, long, default_value = "main", requires = "repo")]
    pub branch: String,
    /// A JavaScript function from this file: a module worker
    /// (`export default { fetch(request, env) { … } }`), up to 1 MB, run in
    /// workerd on your servers (or Cloudflare Workers with `--cloudflare`).
    #[arg(short, long, value_name = "FILE", conflicts_with = "port")]
    pub function: Option<std::path::PathBuf>,
    /// The function runs on Cloudflare Workers (the workspace's Cloudflare
    /// connection) instead of your servers.
    #[arg(long)]
    pub cloudflare: bool,
    /// The Workers runtime version the function is written for.
    #[arg(long, value_name = "YYYY-MM-DD", default_value = DEFAULT_COMPATIBILITY_DATE)]
    pub compatibility_date: String,
    /// The service name (default: derived from the database, image, repo or file).
    #[arg(short, long)]
    pub name: Option<String>,
    /// The port the app listens on (image and repo services; default 8080).
    #[arg(long, conflicts_with = "database")]
    pub port: Option<u16>,
    /// Variables to set in the selected environment (KEY=VALUE, repeatable).
    #[arg(short, long = "variables", value_name = "KEY=VALUE")]
    pub variables: Vec<String>,
}

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("kind").args(["repo", "image", "local"])
))]
pub struct SourceArgs {
    /// Connect this GitHub repository (owner/name), or switch to it.
    #[arg(long)]
    pub repo: Option<String>,
    /// The branch to deploy (default: the current one, else main).
    #[arg(long)]
    pub branch: Option<String>,
    /// Build from this directory of the repository or upload.
    #[arg(long)]
    pub root_dir: Option<String>,
    /// Run this prebuilt image instead.
    #[arg(long)]
    pub image: Option<String>,
    /// Disconnect the repository: deploy local files with `modula up`.
    #[arg(long)]
    pub local: bool,
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
    /// Create an environment and link it. Every service gets a copy there:
    /// databases with an empty volume and new credentials; no domains.
    New {
        /// The new environment's name.
        name: String,
        /// Duplicate this environment: its service settings and all its
        /// variables (sealed ones stay sealed). Without it, the environment is
        /// empty: the default environment's settings, no variables.
        #[arg(short, long, value_name = "ENVIRONMENT")]
        duplicate: Option<String>,
        /// The branch a repo service deploys from there (default: the same
        /// branch). Repeatable.
        #[arg(long = "branch", value_name = "SERVICE=BRANCH")]
        branches: Vec<String>,
    },
    /// Rename an environment.
    Rename {
        /// The environment (name or id).
        environment: String,
        /// Its new name.
        name: String,
    },
    /// Delete an environment with its deployments, variables and volume data
    /// (not the default one). Needs a re-authenticated dashboard session, so
    /// API tokens are refused: use Project Settings → Environments.
    Delete {
        /// The environment (name or id).
        environment: String,
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
        /// Writes need Admin and a recent sign-in, which API tokens never
        /// have: run them from the dashboard's SQL editor instead.
        #[arg(long)]
        write: bool,
    },
    /// List the database's tables.
    Tables,
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    /// Create a Modula server on Hetzner from here: a guided wizard, or
    /// fully from flags for scripts and agents.
    #[command(
        visible_alias = "new",
        long_about = SERVER_CREATE_ABOUT,
        after_help = SERVER_CREATE_EXAMPLES
    )]
    Create(Box<ServerCreateArgs>),
    /// List locations, server types and monthly prices you can create
    /// (read-only; needs HCLOUD_TOKEN).
    #[command(long_about = SERVER_OPTIONS_ABOUT, after_help = SERVER_OPTIONS_EXAMPLES)]
    Options(ServerOptionsArgs),
}

const SERVER_CREATE_ABOUT: &str = "\
Create a Modula server on Hetzner Cloud from your laptop, end to end: a
firewall and an Ubuntu server that installs Modula on first boot, the DNS
record for the dashboard (optionally through Cloudflare), then the admin
account. You end up logged in, ready for `modula init` and `modula up`.

Without flags it's a guided wizard (Esc goes back a step). Every answer can
also be a flag, and with all of them plus --yes nothing is asked: that's how
scripts and AI agents use it (see `modula guide`).

Secrets never go in flags (they'd end up in shell history and process
lists). They come from the environment, or the Hetzner token from stdin:
  HCLOUD_TOKEN           Hetzner Cloud API token (Read & Write), or --token-stdin
  CLOUDFLARE_API_TOKEN   with --cloudflare-dns: Zone → DNS → Edit
  MODULA_ADMIN_PASSWORD  the admin's password (at least 10 characters)
Tokens are only kept in memory while the command runs; nothing but the new
instance's API token (like `modula login`) is written to disk.

If something fails after the server exists, it is deleted with its
firewall (asked first in the wizard; see --on-failure), so nothing keeps
billing.";

const SERVER_CREATE_EXAMPLES: &str = "\
Examples:
  # Guided
  modula server create

  # Scripts and agents: no prompts, JSON progress events on stdout
  export HCLOUD_TOKEN=… MODULA_ADMIN_PASSWORD=…
  modula server options --location fsn1 --json      # pick a type
  modula server create --location fsn1 --type cx33 --name modula-main \\
    --domain deploy.example.com --email you@example.com \\
    --ssh-key-file ~/.ssh/id_ed25519.pub --admin-name 'Ada Lovelace' --yes --json

  # Let Cloudflare create the DNS record (CLOUDFLARE_API_TOKEN set)
  modula server create … --cloudflare-dns --yes

Exit codes: 0 ready and logged in; 1 failed, nothing left running (or it
was deleted); 2 usage: missing or invalid flags, nothing created; 3 a
server exists but isn't fully set up (see the result's hint).";

const SERVER_OPTIONS_ABOUT: &str = "\
List the Hetzner locations where a Modula server can be created right now,
each with the server types in stock there (vCPU, RAM, disk, architecture,
gross monthly price) and the one we recommend. Read-only: nothing is
created. Needs HCLOUD_TOKEN (or --token-stdin).";

const SERVER_OPTIONS_EXAMPLES: &str = "\
Examples:
  HCLOUD_TOKEN=… modula server options
  HCLOUD_TOKEN=… modula server options --location fsn1 --json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Provider {
    #[default]
    Hetzner,
}

/// What to do with a server that was created when a later step fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnFailure {
    /// Ask (the default with a terminal).
    Ask,
    /// Delete the server and its firewall (the default without a terminal).
    Delete,
    /// Keep them (e.g. to look at the install log over SSH).
    Keep,
}

#[derive(Debug, Clone, Default, Args)]
#[command(group(clap::ArgGroup::new("ssh").args(["ssh_key_file", "ssh_key_id", "no_ssh_key"])))]
pub struct ServerCreateArgs {
    /// The cloud provider.
    #[arg(long, value_enum, default_value_t)]
    pub provider: Provider,
    /// Location, e.g. fsn1, nbg1, hel1, ash, hil, sin (see `server options`).
    #[arg(long, value_name = "LOCATION")]
    pub location: Option<String>,
    /// Server type, e.g. cx33 or cax21 (see `server options`).
    #[arg(long = "type", value_name = "TYPE")]
    pub server_type: Option<String>,
    /// The server's name (a DNS label). Default: modula-main.
    #[arg(long)]
    pub name: Option<String>,
    /// The dashboard's hostname, e.g. deploy.example.com. `auto`: no domain
    /// yet, the dashboard at https://<ip> (add a domain there later).
    #[arg(long, value_name = "HOST")]
    pub domain: Option<String>,
    /// For TLS certificates (Let's Encrypt) and the admin account.
    #[arg(long)]
    pub email: Option<String>,
    /// Upload this public key (or reuse it if Hetzner has it) for root SSH.
    #[arg(long, value_name = "PATH")]
    pub ssh_key_file: Option<std::path::PathBuf>,
    /// Use this SSH key already in the Hetzner project (its id).
    #[arg(long, value_name = "ID")]
    pub ssh_key_id: Option<i64>,
    /// No SSH key: Hetzner emails you a root password instead.
    #[arg(long)]
    pub no_ssh_key: bool,
    /// Create the dashboard's A record with Cloudflare (CLOUDFLARE_API_TOKEN).
    #[arg(long)]
    pub cloudflare_dns: bool,
    /// With --cloudflare-dns: replace existing A/AAAA/CNAME records for the domain.
    #[arg(long, requires = "cloudflare_dns")]
    pub replace_dns: bool,
    /// The admin's display name. Default: Admin.
    #[arg(long, value_name = "NAME")]
    pub admin_name: Option<String>,
    /// Read the Hetzner API token from the first line of stdin.
    #[arg(long)]
    pub token_stdin: bool,
    /// When a step fails after the server was created. Default: ask with a
    /// terminal, else delete.
    #[arg(long, value_enum, value_name = "WHAT")]
    pub on_failure: Option<OnFailure>,
    /// Minutes to wait for the domain to point at the server.
    #[arg(long, value_name = "MINUTES", default_value_t = 30)]
    pub dns_timeout: u64,
    /// Minutes to wait for the install to finish.
    #[arg(long, value_name = "MINUTES", default_value_t = 20)]
    pub install_timeout: u64,
    /// Where the server downloads install.sh (testing the installer).
    #[arg(long, hide = true, value_name = "URL")]
    pub install_url: Option<String>,
    /// Rejected: secrets go in HCLOUD_TOKEN or --token-stdin.
    #[arg(long, hide = true, value_name = "TOKEN")]
    pub hcloud_token: Option<String>,
    /// Rejected: secrets go in CLOUDFLARE_API_TOKEN.
    #[arg(long, hide = true, value_name = "TOKEN")]
    pub cloudflare_token: Option<String>,
    /// Rejected: secrets go in MODULA_ADMIN_PASSWORD.
    #[arg(long, hide = true, value_name = "PASSWORD")]
    pub admin_password: Option<String>,
}

#[derive(Debug, Clone, Default, Args)]
pub struct ServerOptionsArgs {
    /// The cloud provider.
    #[arg(long, value_enum, default_value_t)]
    pub provider: Provider,
    /// Only this location, e.g. fsn1.
    #[arg(long, value_name = "LOCATION")]
    pub location: Option<String>,
    /// Read the Hetzner API token from the first line of stdin.
    #[arg(long)]
    pub token_stdin: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("modula").chain(args.iter().copied()))
    }

    #[test]
    fn the_command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn server_create_flags() {
        let cli = parse(&[
            "server",
            "create",
            "--location",
            "fsn1",
            "--type",
            "cx33",
            "--domain",
            "d.example.com",
            "--email",
            "a@b.co",
            "--ssh-key-id",
            "42",
            "--yes",
            "--json",
        ])
        .unwrap();
        assert!(cli.global.yes && cli.global.json);
        let Command::Server {
            command: ServerCommand::Create(a),
        } = cli.command
        else {
            panic!()
        };
        assert_eq!(a.server_type.as_deref(), Some("cx33"));
        assert_eq!(a.ssh_key_id, Some(42));
        assert_eq!((a.dns_timeout, a.install_timeout), (30, 20));
        // One SSH choice at most; --replace-dns only with --cloudflare-dns.
        assert!(parse(&["server", "create", "--ssh-key-id", "1", "--no-ssh-key"]).is_err());
        assert!(parse(&["server", "create", "--replace-dns"]).is_err());
        assert!(parse(&["server", "options", "--location", "hel1"]).is_ok());
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

        let cli = parse(&[
            "env",
            "new",
            "preview",
            "--duplicate",
            "staging",
            "--branch",
            "api=dev",
            "--branch",
            "web=dev",
        ])
        .unwrap();
        let Command::Environment(e) = cli.command else {
            panic!()
        };
        assert!(matches!(
            e.command,
            Some(EnvironmentCommand::New { ref name, ref duplicate, ref branches })
                if name == "preview" && duplicate.as_deref() == Some("staging")
                    && branches == &["api=dev", "web=dev"]
        ));

        let cli = parse(&["env", "rename", "staging", "preview"]).unwrap();
        let Command::Environment(e) = cli.command else {
            panic!()
        };
        assert!(matches!(
            e.command,
            Some(EnvironmentCommand::Rename { ref environment, ref name })
                if environment == "staging" && name == "preview"
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
        let cli = parse(&["add", "--function", "hello.js", "--cloudflare"]).unwrap();
        let Command::Add(a) = cli.command else {
            panic!()
        };
        assert_eq!(
            a.function.as_deref(),
            Some(std::path::Path::new("hello.js"))
        );
        assert!(a.cloudflare);
        assert_eq!(a.compatibility_date, DEFAULT_COMPATIBILITY_DATE);
        assert!(parse(&["add", "--function", "a.js", "--image", "nginx"]).is_err());
        assert!(parse(&["add", "--function", "a.js", "--port", "80"]).is_err());

        let cli = parse(&["add", "--template", "ghost", "--name", "blog"]).unwrap();
        let Command::Add(a) = cli.command else {
            panic!()
        };
        assert_eq!(a.template.as_deref(), Some("ghost"));
        assert!(parse(&["add", "--template", "ghost", "--image", "nginx"]).is_err());
        assert!(parse(&["add", "--template", "n8n", "-v", "A=1"]).is_err());
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
    fn ssh_and_run_take_commands() {
        let cli = parse(&["ssh"]).unwrap();
        assert!(
            matches!(cli.command, Command::Ssh { service: None, ref command, .. } if command.is_empty())
        );
        let cli = parse(&["ssh", "api", "-e", "staging", "--", "ls", "-la"]).unwrap();
        let Command::Ssh {
            service, command, ..
        } = cli.command
        else {
            panic!()
        };
        assert_eq!(service.as_deref(), Some("api"));
        assert_eq!(command, ["ls", "-la"]);
        assert_eq!(cli.global.environment.as_deref(), Some("staging"));
        let cli = parse(&["ssh", "-s", "web", "--replica", "1"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Ssh {
                replica: Some(1),
                ..
            }
        ));
        assert_eq!(cli.global.service.as_deref(), Some("web"));
        // Without `--` a second word is not a command.
        assert!(parse(&["ssh", "api", "ls"]).is_err());

        let cli = parse(&["run", "-s", "api", "--", "npm", "run", "migrate"]).unwrap();
        assert!(
            matches!(cli.command, Command::Run { ref command } if command == &["npm", "run", "migrate"])
        );
        assert_eq!(cli.global.service.as_deref(), Some("api"));
        // Flags after the command belong to it.
        let cli = parse(&["run", "node", "-e", "1"]).unwrap();
        assert!(
            matches!(cli.command, Command::Run { ref command } if command == &["node", "-e", "1"])
        );
        assert!(cli.global.environment.is_none());
        assert!(parse(&["run"]).is_err());
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
