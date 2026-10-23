//! Authorization. Every resource-scoped route declares an [`Action`]; the router
//! resolves the single `{..._id}` path parameter to its workspace and checks the
//! caller's role *before* any handler or body extractor runs.
//!
//! Instance admins get no implicit access to workspaces (SaaS-ready isolation).

use modula_id::Id;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

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

/// Who owns a resource: a workspace (role-based), a single user account, or the
/// instance itself (instance admins only, e.g. nodes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnerKind {
    Workspace,
    Account,
    Instance,
}

/// Every protected operation. Adding a variant forces decisions in the matches below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    WorkspaceRead,
    WorkspaceUpdate,
    WorkspaceDelete,
    /// Revoke one of the caller's own sessions.
    SessionRevoke,
    ProjectCreate,
    ProjectRead,
    ProjectUpdate,
    ProjectDelete,
    EnvironmentCreate,
    EnvironmentDelete,
    /// Copy another environment's config (and missing variables) into this one.
    EnvironmentSync,
    ServiceCreate,
    ServiceDelete,
    InstanceRead,
    InstanceUpdate,
    /// Variable names and metadata (never values).
    VariablesRead,
    /// Set or delete variables (write-only: no read-back).
    VariablesWrite,
    VariableReveal,
    DeploymentCreate,
    DeploymentRead,
    DeploymentRemove,
    /// Redeploy, roll back to, or restart an existing deployment.
    DeploymentRollout,
    DomainCreate,
    DomainDelete,
    /// Instance-level, no path parameter.
    NodeList,
    /// Instance-level, no path parameter.
    NodeJoinTokenCreate,
    NodeRevoke,
    /// Pin a server's location.
    NodeUpdate,
    /// Instance-level, no path parameter: create the instance's GitHub App.
    GithubAppCreate,
    /// Install the GitHub App for a workspace.
    GithubInstall,
    /// List repositories reachable through a workspace's installations.
    GithubRepoList,
}

impl Action {
    /// The resource kind this action is authorized against; `None` = an
    /// instance-level action with no path parameter (instance admins only).
    pub fn kind(self) -> Option<ResourceKind> {
        use ResourceKind as K;
        Some(match self {
            Self::WorkspaceRead | Self::WorkspaceUpdate | Self::WorkspaceDelete => K::Workspace,
            Self::ProjectCreate => K::Workspace,
            Self::SessionRevoke => K::Session,
            Self::ProjectRead | Self::ProjectUpdate | Self::ProjectDelete => K::Project,
            Self::EnvironmentCreate | Self::ServiceCreate => K::Project,
            Self::EnvironmentDelete | Self::EnvironmentSync => K::Environment,
            Self::ServiceDelete => K::Service,
            Self::InstanceRead | Self::InstanceUpdate => K::ServiceInstance,
            Self::VariablesRead | Self::VariablesWrite | Self::DeploymentCreate => {
                K::ServiceInstance
            }
            Self::VariableReveal => K::Variable,
            Self::DomainCreate => K::ServiceInstance,
            Self::DomainDelete => K::Domain,
            Self::DeploymentRead | Self::DeploymentRemove | Self::DeploymentRollout => {
                K::Deployment
            }
            Self::NodeRevoke | Self::NodeUpdate => K::Node,
            Self::GithubInstall | Self::GithubRepoList => K::Workspace,
            Self::NodeList | Self::NodeJoinTokenCreate | Self::GithubAppCreate => return None,
        })
    }

    pub fn owner(self) -> OwnerKind {
        self.kind().map_or(OwnerKind::Instance, ResourceKind::owner)
    }

    /// Minimum workspace role (only meaningful for workspace-owned actions).
    pub fn min_role(self) -> Role {
        match self {
            Self::WorkspaceRead
            | Self::ProjectRead
            | Self::InstanceRead
            | Self::VariablesRead
            | Self::DeploymentRead => Role::Viewer,
            Self::VariablesWrite
            | Self::DeploymentCreate
            | Self::DeploymentRemove
            | Self::DeploymentRollout => Role::Deployer,
            Self::ProjectCreate
            | Self::ProjectUpdate
            | Self::EnvironmentCreate
            | Self::EnvironmentSync
            | Self::ServiceCreate
            | Self::ServiceDelete
            | Self::InstanceUpdate
            | Self::VariableReveal
            | Self::DomainCreate
            | Self::DomainDelete
            | Self::GithubRepoList => Role::Member,
            Self::WorkspaceUpdate
            | Self::WorkspaceDelete
            | Self::ProjectDelete
            | Self::EnvironmentDelete
            | Self::GithubInstall => Role::Admin,
            Self::SessionRevoke
            | Self::NodeList
            | Self::NodeJoinTokenCreate
            | Self::NodeRevoke
            | Self::NodeUpdate
            | Self::GithubAppCreate => Role::Admin,
        }
    }

    /// Destructive or secret-revealing actions need a recent re-authentication.
    pub fn requires_sudo(self) -> bool {
        match self {
            Self::WorkspaceDelete
            | Self::ProjectDelete
            | Self::EnvironmentDelete
            | Self::ServiceDelete
            | Self::VariableReveal
            | Self::NodeJoinTokenCreate
            | Self::NodeRevoke
            | Self::GithubAppCreate => true,
            Self::WorkspaceRead
            | Self::WorkspaceUpdate
            | Self::SessionRevoke
            | Self::ProjectCreate
            | Self::ProjectRead
            | Self::ProjectUpdate
            | Self::EnvironmentCreate
            | Self::EnvironmentSync
            | Self::ServiceCreate
            | Self::InstanceRead
            | Self::InstanceUpdate
            | Self::VariablesRead
            | Self::VariablesWrite
            | Self::DeploymentCreate
            | Self::DeploymentRead
            | Self::DeploymentRemove
            | Self::DeploymentRollout
            | Self::DomainCreate
            | Self::DomainDelete
            | Self::NodeList
            | Self::NodeUpdate
            | Self::GithubInstall
            | Self::GithubRepoList => false,
        }
    }
}

/// Resource types addressable by a path parameter. Param names are fixed so the
/// router can map `{workspace_id}` → [`ResourceKind::Workspace`] mechanically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Workspace,
    Session,
    Project,
    Environment,
    Service,
    ServiceInstance,
    Variable,
    Deployment,
    Domain,
    Node,
}

impl ResourceKind {
    pub const ALL: &'static [ResourceKind] = &[
        ResourceKind::Workspace,
        ResourceKind::Session,
        ResourceKind::Project,
        ResourceKind::Environment,
        ResourceKind::Service,
        ResourceKind::ServiceInstance,
        ResourceKind::Variable,
        ResourceKind::Deployment,
        ResourceKind::Domain,
        ResourceKind::Node,
    ];

    pub fn param(self) -> &'static str {
        match self {
            Self::Workspace => "workspace_id",
            Self::Session => "session_id",
            Self::Project => "project_id",
            Self::Environment => "environment_id",
            Self::Service => "service_id",
            Self::ServiceInstance => "instance_id",
            Self::Variable => "variable_id",
            Self::Deployment => "deployment_id",
            Self::Domain => "domain_id",
            Self::Node => "node_id",
        }
    }

    pub fn owner(self) -> OwnerKind {
        match self {
            Self::Session => OwnerKind::Account,
            Self::Node => OwnerKind::Instance,
            Self::Workspace
            | Self::Project
            | Self::Environment
            | Self::Service
            | Self::ServiceInstance
            | Self::Variable
            | Self::Deployment
            | Self::Domain => OwnerKind::Workspace,
        }
    }

    pub fn from_param(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.param() == name)
    }

    /// The id of the workspace or user that owns resource `id`, if it exists.
    /// Instance-owned resources return the all-zero id.
    async fn owner_id(self, db: &PgPool, id: Id) -> AppResult<Option<Id>> {
        let sql = match self {
            Self::Workspace => "SELECT id FROM workspaces WHERE id = $1",
            Self::Session => "SELECT user_id FROM sessions WHERE id = $1 AND kind = 'full'",
            Self::Project => "SELECT workspace_id FROM projects WHERE id = $1",
            Self::Environment => {
                "SELECT p.workspace_id FROM environments e
                 JOIN projects p ON p.id = e.project_id WHERE e.id = $1"
            }
            Self::Service => {
                "SELECT p.workspace_id FROM services s
                 JOIN projects p ON p.id = s.project_id WHERE s.id = $1"
            }
            Self::ServiceInstance => {
                "SELECT p.workspace_id FROM service_instances i
                 JOIN services s ON s.id = i.service_id
                 JOIN projects p ON p.id = s.project_id WHERE i.id = $1"
            }
            Self::Variable => {
                "SELECT p.workspace_id FROM variables v
                 JOIN service_instances i ON i.id = v.service_instance_id
                 JOIN services s ON s.id = i.service_id
                 JOIN projects p ON p.id = s.project_id WHERE v.id = $1"
            }
            Self::Deployment => {
                "SELECT p.workspace_id FROM deployments d
                 JOIN service_instances i ON i.id = d.service_instance_id
                 JOIN services s ON s.id = i.service_id
                 JOIN projects p ON p.id = s.project_id WHERE d.id = $1"
            }
            Self::Domain => {
                "SELECT p.workspace_id FROM domains m
                 JOIN service_instances i ON i.id = m.service_instance_id
                 JOIN services s ON s.id = i.service_id
                 JOIN projects p ON p.id = s.project_id WHERE m.id = $1"
            }
            Self::Node => "SELECT '0000000000000000' FROM nodes WHERE id = $1",
        };
        Ok(sqlx::query_scalar(sql).bind(id).fetch_optional(db).await?)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Resource {
    pub kind: ResourceKind,
    pub id: Id,
}

#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Workspace { id: Id, role: Role },
    Account { user_id: Id },
    Instance,
}

/// Proof that the actor may perform the route's action on `resource`.
#[derive(Debug, Clone, Copy)]
pub struct Grant {
    pub scope: Scope,
    pub resource: Resource,
}

impl Grant {
    /// The id from the route's path parameter.
    pub fn id(&self) -> Id {
        self.resource.id
    }

    pub fn workspace_id(&self) -> AppResult<Id> {
        match self.scope {
            Scope::Workspace { id, .. } => Ok(id),
            Scope::Account { .. } | Scope::Instance => {
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
    if action.kind() != Some(resource.kind) {
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
        // Instance-owned resources are invisible to everyone but instance admins.
        OwnerKind::Instance if actor.is_instance_admin => Scope::Instance,
        OwnerKind::Instance => return Err(AppError::NotFound),
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
    fn every_action_targets_a_matching_kind() {
        for k in ResourceKind::ALL {
            assert_eq!(ResourceKind::from_param(k.param()), Some(*k));
        }
        // Instance-level actions are admin-only.
        for a in [
            Action::NodeList,
            Action::NodeJoinTokenCreate,
            Action::NodeRevoke,
            Action::GithubAppCreate,
        ] {
            assert_eq!(a.owner(), OwnerKind::Instance);
        }
    }

    #[test]
    fn params_round_trip() {
        for k in ResourceKind::ALL {
            assert_eq!(ResourceKind::from_param(k.param()), Some(*k));
            assert!(k.param().ends_with("_id"));
        }
    }
}
