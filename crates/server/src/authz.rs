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

/// Who owns a resource: a workspace (role-based) or a single user account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnerKind {
    Workspace,
    Account,
}

/// Every protected operation. Adding a variant forces decisions in the matches below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    WorkspaceRead,
    WorkspaceUpdate,
    WorkspaceDelete,
    /// Revoke one of the caller's own sessions.
    SessionRevoke,
}

impl Action {
    pub fn owner(self) -> OwnerKind {
        match self {
            Self::WorkspaceRead | Self::WorkspaceUpdate | Self::WorkspaceDelete => {
                OwnerKind::Workspace
            }
            Self::SessionRevoke => OwnerKind::Account,
        }
    }

    /// Minimum workspace role (only meaningful for workspace-owned actions).
    pub fn min_role(self) -> Role {
        match self {
            Self::WorkspaceRead => Role::Viewer,
            Self::WorkspaceUpdate | Self::WorkspaceDelete => Role::Admin,
            Self::SessionRevoke => Role::Admin,
        }
    }

    /// Destructive or secret-revealing actions need a recent re-authentication.
    pub fn requires_sudo(self) -> bool {
        match self {
            Self::WorkspaceDelete => true,
            Self::WorkspaceRead | Self::WorkspaceUpdate | Self::SessionRevoke => false,
        }
    }
}

/// Resource types addressable by a path parameter. Param names are fixed so the
/// router can map `{workspace_id}` → [`ResourceKind::Workspace`] mechanically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Workspace,
    Session,
}

impl ResourceKind {
    pub const ALL: &'static [ResourceKind] = &[ResourceKind::Workspace, ResourceKind::Session];

    pub fn param(self) -> &'static str {
        match self {
            Self::Workspace => "workspace_id",
            Self::Session => "session_id",
        }
    }

    pub fn owner(self) -> OwnerKind {
        match self {
            Self::Workspace => OwnerKind::Workspace,
            Self::Session => OwnerKind::Account,
        }
    }

    pub fn from_param(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.param() == name)
    }

    /// The id of the workspace or user that owns resource `id`, if it exists.
    async fn owner_id(self, db: &PgPool, id: Uuid) -> AppResult<Option<Uuid>> {
        let sql = match self {
            Self::Workspace => "SELECT id FROM workspaces WHERE id = $1",
            Self::Session => "SELECT user_id FROM sessions WHERE id = $1 AND kind = 'full'",
        };
        Ok(sqlx::query_scalar(sql).bind(id).fetch_optional(db).await?)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Resource {
    pub kind: ResourceKind,
    pub id: Uuid,
}

#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Workspace { id: Uuid, role: Role },
    Account { user_id: Uuid },
}

/// Proof that the actor may perform the route's action on `resource`.
#[derive(Debug, Clone, Copy)]
pub struct Grant {
    pub scope: Scope,
    pub resource: Resource,
}

impl Grant {
    pub fn workspace_id(&self) -> AppResult<Uuid> {
        match self.scope {
            Scope::Workspace { id, .. } => Ok(id),
            Scope::Account { .. } => {
                Err(AppError::Internal(anyhow::anyhow!("not a workspace grant")))
            }
        }
    }
}

/// Non-owners/non-members get 404 (existence is not leaked); members lacking
/// the role get 403. Instance admins get no implicit access.
pub async fn authorize(
    db: &PgPool,
    actor: &Actor,
    action: Action,
    resource: Resource,
) -> AppResult<Grant> {
    if action.owner() != resource.kind.owner() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "{action:?} cannot apply to {:?}",
            resource.kind
        )));
    }
    let Some(owner) = resource.kind.owner_id(db, resource.id).await? else {
        return Err(AppError::NotFound);
    };
    let scope = match action.owner() {
        OwnerKind::Account if owner == actor.user_id => Scope::Account { user_id: owner },
        OwnerKind::Account => return Err(AppError::NotFound),
        OwnerKind::Workspace => {
            let role: Option<String> = sqlx::query_scalar(
                "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
            )
            .bind(owner)
            .bind(actor.user_id)
            .fetch_optional(db)
            .await?;
            let Some(role) = role.as_deref().and_then(Role::parse) else {
                return Err(AppError::NotFound);
            };
            if role < action.min_role() {
                return Err(AppError::Forbidden);
            }
            Scope::Workspace { id: owner, role }
        }
    };
    Ok(Grant { scope, resource })
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
