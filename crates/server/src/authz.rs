//! Authorization. Every resource-scoped route declares an [`Action`]; the router
//! resolves the single `{..._id}` path parameter to its workspace and checks the
//! caller's role *before* any handler or body extractor runs.
//!
//! Instance admins get no implicit access to workspaces (SaaS-ready isolation).

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::session::Actor;
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Deployer,
    Member,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Deployer => "deployer",
            Self::Member => "member",
            Self::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "viewer" => Self::Viewer,
            "deployer" => Self::Deployer,
            "member" => Self::Member,
            "admin" => Self::Admin,
            _ => return None,
        })
    }
}

/// Every protected operation. Adding a variant forces a decision in `min_role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    WorkspaceRead,
    WorkspaceUpdate,
    WorkspaceDelete,
}

impl Action {
    pub fn min_role(self) -> Role {
        match self {
            Self::WorkspaceRead => Role::Viewer,
            Self::WorkspaceUpdate | Self::WorkspaceDelete => Role::Admin,
        }
    }
}

/// Resource types addressable by a path parameter. Param names are fixed so the
/// router can map `{workspace_id}` → [`ResourceKind::Workspace`] mechanically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Workspace,
}

impl ResourceKind {
    pub const ALL: &'static [ResourceKind] = &[ResourceKind::Workspace];

    pub fn param(self) -> &'static str {
        match self {
            Self::Workspace => "workspace_id",
        }
    }

    pub fn from_param(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.param() == name)
    }

    /// The workspace that owns resource `id`, if it exists.
    async fn owning_workspace(self, db: &PgPool, id: Uuid) -> AppResult<Option<Uuid>> {
        let sql = match self {
            Self::Workspace => "SELECT id FROM workspaces WHERE id = $1",
        };
        Ok(sqlx::query_scalar(sql).bind(id).fetch_optional(db).await?)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Resource {
    pub kind: ResourceKind,
    pub id: Uuid,
}

/// Proof that the actor may perform the route's action on `resource`.
#[derive(Debug, Clone, Copy)]
pub struct Grant {
    pub workspace_id: Uuid,
    pub role: Role,
    pub resource: Resource,
}

/// Non-members get 404 (existence is not leaked); members lacking the role get 403.
pub async fn authorize(
    db: &PgPool,
    actor: &Actor,
    action: Action,
    resource: Resource,
) -> AppResult<Grant> {
    let Some(workspace_id) = resource.kind.owning_workspace(db, resource.id).await? else {
        return Err(AppError::NotFound);
    };
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id)
    .bind(actor.user_id)
    .fetch_optional(db)
    .await?;
    let Some(role) = role.as_deref().and_then(Role::parse) else {
        return Err(AppError::NotFound);
    };
    if role < action.min_role() {
        return Err(AppError::Forbidden);
    }
    Ok(Grant {
        workspace_id,
        role,
        resource,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_order() {
        assert!(Role::Viewer < Role::Deployer);
        assert!(Role::Deployer < Role::Member);
        assert!(Role::Member < Role::Admin);
        for r in [Role::Viewer, Role::Deployer, Role::Member, Role::Admin] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Role::parse("owner"), None);
    }

    #[test]
    fn params_round_trip() {
        for k in ResourceKind::ALL {
            assert_eq!(ResourceKind::from_param(k.param()), Some(*k));
            assert!(k.param().ends_with("_id"));
        }
    }
}
