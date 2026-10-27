//! `volume` and `tcp-proxy`. Both take effect on the next deploy (volumes)
//! or right away (TCP proxies, on the server running the service).

use crate::cli::{TcpProxyCommand, VolumeCommand};
use crate::context::Ctx;

pub async fn volume(ctx: &Ctx, command: Option<VolumeCommand>) -> anyhow::Result<()> {
    let t = ctx.target().await?;
    let api = ctx.api()?;
    match command {
        None => {
            let i = api.instance(&t.instance).await?;
            ctx.out.either(&i.volume, |o| match &i.volume {
                Some(v) => o.line(format!(
                    "{} is mounted at {} ({})",
                    t.label(),
                    v.mount_path,
                    v.id
                )),
                None => o.line(format!("{} has no volume.", t.label())),
            })
        }
        Some(VolumeCommand::Add { path }) => {
            let i = api.create_volume(&t.instance, &path).await?;
            ctx.out.either(&i.volume, |o| {
                o.line(format!("Added a volume at {path} to {}", t.label()));
                o.note("It is attached on the next deploy: run `modula deploy`.");
            })
        }
        Some(VolumeCommand::Delete) => {
            let i = api.instance(&t.instance).await?;
            let v = i
                .volume
                .ok_or_else(|| anyhow::anyhow!("{} has no volume", t.label()))?;
            ctx.confirm(&format!(
                "Delete the volume at {} of {} and all its data?",
                v.mount_path,
                t.label()
            ))?;
            let r = api.delete_volume(&v.id).await?;
            ctx.out.either(&r, |o| {
                o.line(format!("Deleted the volume at {}", v.mount_path))
            })
        }
    }
}

pub async fn tcp_proxy(ctx: &Ctx, command: Option<TcpProxyCommand>) -> anyhow::Result<()> {
    let t = ctx.target().await?;
    let api = ctx.api()?;
    let i = api.instance(&t.instance).await?;
    match command {
        None => ctx.out.either(&i.tcp_proxy, |o| match &i.tcp_proxy {
            Some(p) => o.line(format!(
                "{}:{} → port {} of {}",
                p.host,
                p.public_port,
                p.target_port,
                t.label()
            )),
            None => o.line(format!("{} has no TCP proxy.", t.label())),
        }),
        Some(TcpProxyCommand::Enable { port }) => {
            let port = port
                .or_else(|| i.port())
                .ok_or_else(|| anyhow::anyhow!("pass --port: the service has no port set"))?;
            let i = api.create_tcp_proxy(&t.instance, port).await?;
            ctx.out.either(&i.tcp_proxy, |o| {
                if let Some(p) = &i.tcp_proxy {
                    o.line(format!(
                        "{}:{} → port {}",
                        p.host, p.public_port, p.target_port
                    ));
                }
                o.note("This is reachable from the internet; `modula connect` needs no proxy.");
            })
        }
        Some(TcpProxyCommand::Disable) => {
            let p = i
                .tcp_proxy
                .ok_or_else(|| anyhow::anyhow!("{} has no TCP proxy", t.label()))?;
            ctx.confirm(&format!(
                "Remove the TCP proxy {}:{} of {}?",
                p.host,
                p.public_port,
                t.label()
            ))?;
            let r = api.delete_tcp_proxy(&p.id).await?;
            ctx.out.either(&r, |o| {
                o.line(format!("Removed the TCP proxy of {}", t.label()))
            })
        }
    }
}
