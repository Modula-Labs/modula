//! `members`: list a workspace's members, create invite links, remove members.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::ApiError;
use crate::cli::MembersCommand;
use crate::context::Ctx;
use crate::output::Table;
use crate::resolve;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub email: String,
    pub role: String,
    pub joined_at: String,
    pub two_factor: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedInvite {
    pub invite: Value,
    pub url: String,
}

pub async fn run(
    ctx: &Ctx,
    workspace: Option<&str>,
    command: Option<MembersCommand>,
) -> anyhow::Result<()> {
    let api = ctx.api()?;
    let me = api.me().await?;
    let ws = resolve::pick_workspace(&me.workspaces, workspace)?;
    let members_path = format!("/api/workspaces/{}/members", ws.id);
    match command.unwrap_or(MembersCommand::List) {
        MembersCommand::List => {
            let members: Vec<Member> = api.get(&members_path).await?;
            ctx.out.either(&members, |o| {
                let mut t = Table::new(&["name", "email", "role", "2fa", "joined"]);
                for m in &members {
                    t.row([
                        m.name.as_str(),
                        &m.email,
                        &m.role,
                        if m.two_factor { "on" } else { "off" },
                        m.joined_at.get(..10).unwrap_or(&m.joined_at),
                    ]);
                }
                o.table(&t);
            })
        }
        MembersCommand::Invite {
            role,
            expires_in_days,
        } => {
            let mut body = json!({ "role": role });
            if let Some(d) = expires_in_days {
                body["expiresInDays"] = json!(d);
            }
            let created: CreatedInvite = api
                .post(&format!("/api/workspaces/{}/invites", ws.id), &body)
                .await?;
            ctx.out.either(&created, |o| {
                o.line(&created.url);
                o.note(format!(
                    "Single-use link to join {} as {role}; it is shown only now.",
                    ws.name
                ));
            })
        }
        MembersCommand::Remove { email } => {
            let members: Vec<Member> = api.get(&members_path).await?;
            let wanted = email.trim().to_lowercase();
            let m = members
                .iter()
                .find(|m| m.email == wanted)
                .with_context(|| format!("{email} is not a member of {}", ws.name))?;
            ctx.confirm(&format!(
                "Remove {} ({}) from {}?",
                m.name, m.email, ws.name
            ))?;
            if let Err(e) = api.delete::<Value>(&format!("/api/members/{}", m.id)).await {
                let sudo = e
                    .downcast_ref::<ApiError>()
                    .is_some_and(|a| a.code == "sudo_required");
                if sudo {
                    let url = api.login.url.join("settings#members")?;
                    return Err(e).context(format!("remove them in the dashboard: {url}"));
                }
                return Err(e);
            }
            ctx.out.either(&json!({ "ok": true }), |o| {
                o.line(format!("Removed {} from {}", m.email, ws.name));
            })
        }
    }
}
