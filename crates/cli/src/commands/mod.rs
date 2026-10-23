//! One module per command group; [`run`] dispatches a parsed command line.

pub mod auth;
pub mod config;
pub mod db;
pub mod deploy;
pub mod domain;
pub mod environment;
pub mod logs;
pub mod meta;
pub mod project;
pub mod service;
pub mod storage;
pub mod up;
pub mod variables;

use crate::cli::{Cli, Command};
use crate::context::{Ctx, Session};
use crate::output::Output;

/// Run one command; returns the process exit code.
pub async fn run(cli: Cli, session: Session, out: Output) -> anyhow::Result<i32> {
    let mut ctx = Ctx::new(session, cli.global, out)?;
    let ctx = &mut ctx;
    match cli.command {
        Command::Login { url, token, local } => auth::login(ctx, url, token, local).await?,
        Command::Logout => auth::logout(ctx)?,
        Command::Whoami => auth::whoami(ctx).await?,
        Command::List => project::list(ctx).await?,
        Command::Init { name, workspace } => {
            project::init(ctx, &name, workspace.as_deref()).await?
        }
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
            return deploy::rollback(ctx, &deployment, wait).await;
        }
        Command::Deployment { command } => return deploy::deployment(ctx, command).await,
        Command::Logs(args) => return logs::run(ctx, args).await,
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
        Command::Completion { shell } => meta::completion(ctx, shell),
        Command::Docs => meta::docs(ctx),
        Command::Guide => meta::guide(ctx),
        Command::Server { command } => return crate::provision::run(ctx, command).await,
        Command::Config { command } => return config::run(ctx, command).await,
    }
    Ok(0)
}
