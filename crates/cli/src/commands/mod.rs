//! One module per command group; [`run`] dispatches a parsed command line.

pub mod auth;
pub mod config;
pub mod cron;
pub mod db;
pub mod deploy;
pub mod domain;
pub mod environment;
pub mod init;
pub mod instance;
pub mod logs;
pub mod members;
pub mod meta;
pub mod metrics;
pub mod project;
pub mod service;
pub mod shell;
pub mod source;
pub mod storage;
pub mod up;
pub mod update;
pub mod variables;

use crate::cli::{Cli, Command};
use crate::context::{Ctx, Session};
use crate::output::Output;

/// Run one command; returns the process exit code.
pub async fn run(cli: Cli, session: Session, out: Output) -> anyhow::Result<i32> {
    crate::config::set_allow_insecure_http(cli.global.allow_insecure_http);
    // Pickers only for a person at a terminal (not scripts, agents or CI).
    crate::resolve::set_interactive(
        session.interactive
            && !cli.global.yes
            && !cli.global.json
            && std::io::IsTerminal::is_terminal(&std::io::stdout())
            && std::env::var_os("CI").is_none(),
    );
    let mut ctx = Ctx::new(session, cli.global, out)?;
    let ctx = &mut ctx;
    // The daily "a newer version is available" notice (not around updating).
    let notifier = match &cli.command {
        Command::Update { .. }
        | Command::UpdateCheck
        | Command::Server {
            command: crate::cli::ServerCommand::Update { .. },
        }
        | Command::Completion { .. }
        | Command::Docs
        | Command::Guide => None,
        _ => crate::update::Notifier::start(ctx),
    };
    let result = dispatch(ctx, cli.command).await;
    if let (Some(n), Ok(_)) = (notifier, &result) {
        n.finish(&ctx.out);
    }
    result
}

async fn dispatch(ctx: &mut Ctx, command: Command) -> anyhow::Result<i32> {
    match command {
        Command::Login { args, command } => auth::run(ctx, args, command).await?,
        Command::Logins => auth::list(ctx)?,
        Command::Logout { name, all } => auth::logout(ctx, name, all)?,
        Command::Whoami => auth::whoami(ctx).await?,
        Command::List => project::list(ctx).await?,
        Command::Init {
            name,
            workspace,
            format,
        } => init::run(ctx, name, workspace, format).await?,
        Command::Link { token } => {
            if let Some(token) = token {
                // Here, where the link goes.
                ctx.link = None;
                auth::save_local_token(ctx, token).await?;
            }
            project::link(ctx).await?
        }
        Command::Unlink => project::unlink(ctx)?,
        Command::Status => project::status(ctx).await?,
        Command::Open => project::open(ctx).await?,
        Command::Service { name } => project::service(ctx, name.as_deref()).await?,
        Command::Add(args) => service::add(ctx, args).await?,
        Command::Up(args) => return up::run(ctx, args).await,
        Command::Deploy { all, wait } => return deploy::deploy(ctx, all, wait).await,
        Command::Redeploy { wait } => return deploy::rollout(ctx, "redeploy", wait).await,
        Command::Restart => return deploy::rollout(ctx, "restart", false).await,
        Command::Down => return deploy::down(ctx).await,
        Command::Rollback { deployment, wait } => {
            return deploy::rollback(ctx, deployment.as_deref(), wait).await;
        }
        Command::Deployment { command } => return deploy::deployment(ctx, command).await,
        Command::Logs(args) => return logs::run(ctx, args).await,
        Command::Metrics { range } => return metrics::run(ctx, &range).await,
        Command::Source(args) => source::run(ctx, args).await?,
        Command::Variables(args) => return variables::run(ctx, args).await,
        Command::Environment(args) => environment::run(ctx, args).await?,
        Command::Domain(args) => domain::run(ctx, args).await?,
        Command::Volume { command } => storage::volume(ctx, command).await?,
        Command::TcpProxy { command } => storage::tcp_proxy(ctx, command).await?,
        Command::Db { command } => db::run(ctx, command).await?,
        Command::Connect {
            service,
            tunnel_only,
            show_password,
            port,
        } => return db::connect(ctx, service, tunnel_only, show_password, port).await,
        Command::Ssh {
            service,
            replica,
            command,
        } => return shell::ssh(ctx, service, replica, command).await,
        Command::Run { command } => return shell::run(ctx, command).await,
        Command::Completion { shell } => meta::completion(ctx, shell),
        Command::Docs => meta::docs(ctx),
        Command::Guide => meta::guide(ctx),
        Command::Server { command } => return crate::provision::run(ctx, command).await,
        Command::Config { command } => return config::run(ctx, command).await,
        Command::Instance { command } => instance::run(ctx, command).await?,
        Command::Members { workspace, command } => {
            members::run(ctx, workspace.as_deref(), command).await?
        }
        Command::Cron { command } => return cron::run(ctx, command).await,
        Command::Update { check } => return update::cli(ctx, check).await,
        Command::UpdateCheck => crate::update::background_check(ctx).await,
    }
    Ok(0)
}
