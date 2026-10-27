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
    /// Rename or remove one of the caller's own passkeys.
    PasskeyRename,
    PasskeyDelete,
    ProjectCreate,
    ProjectRead,
    ProjectUpdate,
    ProjectDelete,
    EnvironmentCreate,
    /// Rename.
    EnvironmentUpdate,
    EnvironmentDelete,
    /// Copy another environment's config (and missing variables) into this one.
    EnvironmentSync,
    ServiceCreate,
    /// Its display name.
    ServiceUpdate,
    /// Every environment's instance and the service itself.
    ServiceDelete,
    /// One environment's instance (the service goes with its last one).
    InstanceDelete,
    InstanceRead,
    InstanceUpdate,
    /// Variable names and metadata (never values).
    VariablesRead,
    /// Set or delete variables (write-only: no read-back).
    VariablesWrite,
    VariableReveal,
    /// An environment's shared variables (names and metadata) and the names
    /// references can point at.
    SharedVariablesRead,
    SharedVariablesWrite,
    /// The whole environment as config as code (`modula config`):
    /// every service's settings, domains, volume and variables, with the
    /// values of non-sealed variables. Audited.
    EnvironmentConfigRead,
    /// Deploy the services with pending changes in an environment.
    EnvironmentDeploy,
    /// Revert pending changes to what was last deployed (variables and settings).
    EnvironmentDiscard,
    DeploymentCreate,
    DeploymentRead,
    DeploymentRemove,
    /// Redeploy, roll back to, or restart an existing deployment.
    DeploymentRollout,
    DomainCreate,
    /// Check a custom domain's DNS now, or change its port and edge settings.
    DomainUpdate,
    DomainDelete,
    /// Attach a volume / expose a TCP port (service instance).
    VolumeCreate,
    /// Deletes the volume's data.
    VolumeDelete,
    TcpProxyCreate,
    TcpProxyDelete,
    /// Create or link an R2 bucket for an R2 service instance (Cloudflare).
    BucketProvision,
    /// Revoke an R2 instance's bucket token and forget the bucket (kept).
    BucketUnlink,
    /// Delete an R2 instance's bucket at Cloudflare.
    BucketDelete,
    /// Instance-level, no path parameter.
    NodeList,
    /// Instance-level, no path parameter.
    NodeJoinTokenCreate,
    NodeRevoke,
    /// Pin a server's location.
    NodeUpdate,
    /// What migrating a server would move, and its migrations.
    NodeMigrationRead,
    /// Migrate a worker server to a new one (creates a server at the
    /// provider, at the expense of the server's workspace).
    NodeMigrate,
    /// A server migration's progress.
    ServerMigrationRead,
    /// Stop a server migration and move its services back.
    ServerMigrationAbort,
    /// Move a service, its volumes' data included, to another server.
    InstanceMove,
    InstanceMoveRead,
    InstanceMoveAbort,
    /// A server's own usage (CPU, memory, network, disk).
    NodeRead,
    /// The sizes a server can change to in place (looks it up at the
    /// provider and links it when needed).
    NodeResizeOptions,
    /// Change a server's size in place (restarts it; can cost money).
    NodeResize,
    /// Instance-level, no path parameter: create the instance's GitHub App.
    GithubAppCreate,
    /// Instance-level: remove one of the instance's GitHub Apps.
    GithubAppDelete,
    /// Install the GitHub App for a workspace.
    GithubInstall,
    /// List repositories reachable through a workspace's installations.
    GithubRepoList,
    /// A workspace's backup storage targets (never their secrets).
    BackupTargetList,
    BackupTargetCreate,
    BackupTargetDelete,
    /// A service instance's backup schedule and backups.
    BackupRead,
    BackupScheduleUpdate,
    /// Back up now.
    BackupCreate,
    /// Replace a database's data with a backup.
    BackupRestore,
    BackupDelete,
    /// Instance-level, no path parameter: the control plane's own backups.
    SelfBackupRead,
    SelfBackupUpdate,
    /// Instance-level, no path parameter: versions, update settings and
    /// upgrade history; check for a new release now.
    UpdatesRead,
    /// Change the update mode and maintenance window.
    UpdatesUpdate,
    /// Upgrade the instance now (or resume a paused server rollout).
    UpdatesRun,
    /// Instance-level, no path parameter: the private dashboard (Cloudflare
    /// Tunnel + Access) status.
    PrivateAccessRead,
    /// Turn the private dashboard on or off.
    PrivateAccessUpdate,
    /// Delete one of the caller's own API tokens.
    TokenRevoke,
    /// Open a private TCP tunnel to a service instance's running container.
    TunnelOpen,
    /// A workspace's Cloudflare connection (never the token) and its zones.
    CloudflareRead,
    /// Verify and store (or replace) a workspace's Cloudflare token.
    CloudflareConnect,
    CloudflareDisconnect,
    /// Database tab (Postgres services): browse tables and rows, read-only
    /// queries, stats, extensions.
    DatabaseRead,
    /// Run SQL, edit rows, vacuum, install or remove extensions.
    DatabaseWrite,
    /// Any database's connection credentials (full access to its data).
    DatabaseCredentials,
    /// Regenerate the database password.
    DatabasePassword,
    /// Restore a Postgres instance to a point in time (into a new service or
    /// over its own data).
    PitrRestore,
    /// Add a read replica of a Postgres instance (a new service).
    ReplicaCreate,
    /// Turn a read replica into a standalone primary.
    ReplicaPromote,
    /// A workspace's Hetzner connection (never the token), caps and usage.
    HetznerRead,
    /// Verify and store (or replace) the Hetzner token; change the caps.
    HetznerConnect,
    HetznerDisconnect,
    /// Hetzner locations, server types with prices, and SSH keys.
    HetznerCatalog,
    /// Servers created through a provider, and their progress.
    ServerList,
    ServerRead,
    /// Create a server at the provider (costs money).
    ServerProvision,
    /// Which server has room for a service, or which to add.
    ServerSuggest,
    /// Delete a provisioned server at the provider and remove its node.
    ServerDelete,
    /// Instance-level: the dashboard's domain and the apps domain.
    InstanceDomainRead,
    InstanceDomainUpdate,
    /// Search an environment's stored logs (every service in it).
    EnvironmentLogsRead,
    /// A project's outgoing webhooks (list, add, send a test, remove).
    WebhookList,
    WebhookCreate,
    WebhookTest,
    WebhookDelete,
    /// A workspace's members (name, email, role, 2FA on or off).
    MemberList,
    /// Create invite links and list the pending ones.
    MemberInvite,
    /// Revoke a pending invite link.
    InviteRevoke,
    /// Change a member's role.
    MemberUpdate,
    /// Remove a member (and their API tokens limited to the workspace).
    MemberRemove,
    /// Leave a workspace yourself.
    MemberLeave,
    /// Open a shell or run a command in a deployment's running container.
    DeploymentExec,
    /// A service instance's resolved variables, with values, for running a
    /// command locally (`modula run`).
    RunEnvRead,
    /// A cron service's schedule (next due times) and its runs.
    CronRunsRead,
    /// Run a cron service now.
    CronRunCreate,
    /// A workspace's alert channels (Slack, Discord, webhooks; never their URLs).
    NotificationChannelList,
    NotificationChannelCreate,
    /// Send a test notification now.
    NotificationChannelTest,
    NotificationChannelDelete,
    /// A workspace's alert rules and the last notifications sent.
    AlertRuleList,
    AlertRuleCreate,
    AlertRuleUpdate,
    AlertRuleDelete,
    AlertHistoryRead,
    /// The alert rules that apply to a project.
    ProjectAlertRuleList,
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
            Self::PasskeyRename | Self::PasskeyDelete => K::Passkey,
            Self::ProjectRead | Self::ProjectUpdate | Self::ProjectDelete => K::Project,
            Self::EnvironmentCreate | Self::ServiceCreate => K::Project,
            Self::EnvironmentDelete
            | Self::EnvironmentUpdate
            | Self::EnvironmentSync
            | Self::SharedVariablesRead
            | Self::SharedVariablesWrite
            | Self::EnvironmentConfigRead
            | Self::EnvironmentDeploy
            | Self::EnvironmentDiscard => K::Environment,
            Self::ServiceUpdate | Self::ServiceDelete => K::Service,
            Self::InstanceRead | Self::InstanceUpdate | Self::InstanceDelete => K::ServiceInstance,
            Self::VariablesRead | Self::VariablesWrite | Self::DeploymentCreate => {
                K::ServiceInstance
            }
            Self::VariableReveal => K::Variable,
            Self::DomainCreate => K::ServiceInstance,
            Self::DomainUpdate | Self::DomainDelete => K::Domain,
            Self::VolumeCreate | Self::TcpProxyCreate => K::ServiceInstance,
            Self::VolumeDelete => K::Volume,
            Self::TcpProxyDelete => K::TcpProxy,
            Self::BucketProvision | Self::BucketUnlink | Self::BucketDelete => K::ServiceInstance,
            Self::DeploymentRead | Self::DeploymentRemove | Self::DeploymentRollout => {
                K::Deployment
            }
            Self::NodeRevoke
            | Self::NodeUpdate
            | Self::NodeRead
            | Self::NodeResizeOptions
            | Self::NodeResize => K::Node,
            Self::NodeMigrationRead | Self::NodeMigrate => K::Node,
            Self::ServerMigrationRead | Self::ServerMigrationAbort => K::ServerMigration,
            Self::InstanceMove => K::ServiceInstance,
            Self::InstanceMoveRead | Self::InstanceMoveAbort => K::InstanceMove,
            Self::GithubInstall | Self::GithubRepoList => K::Workspace,
            Self::NodeList
            | Self::NodeJoinTokenCreate
            | Self::GithubAppCreate
            | Self::GithubAppDelete => return None,
            Self::BackupTargetList | Self::BackupTargetCreate => K::Workspace,
            Self::BackupTargetDelete => K::BackupTarget,
            Self::BackupRead | Self::BackupScheduleUpdate | Self::BackupCreate => {
                K::ServiceInstance
            }
            Self::BackupRestore | Self::BackupDelete => K::Backup,
            Self::SelfBackupRead | Self::SelfBackupUpdate => return None,
            Self::UpdatesRead | Self::UpdatesUpdate | Self::UpdatesRun => return None,
            Self::PrivateAccessRead | Self::PrivateAccessUpdate => return None,
            Self::TokenRevoke => K::ApiToken,
            Self::TunnelOpen => K::ServiceInstance,
            Self::CloudflareRead | Self::CloudflareConnect | Self::CloudflareDisconnect => {
                K::Workspace
            }
            Self::DatabaseRead
            | Self::DatabaseWrite
            | Self::DatabaseCredentials
            | Self::DatabasePassword
            | Self::PitrRestore
            | Self::ReplicaCreate
            | Self::ReplicaPromote => K::ServiceInstance,
            Self::HetznerRead
            | Self::HetznerConnect
            | Self::HetznerDisconnect
            | Self::HetznerCatalog
            | Self::ServerList
            | Self::ServerProvision
            | Self::ServerSuggest => K::Workspace,
            Self::ServerRead | Self::ServerDelete => K::ProvisionedServer,
            Self::InstanceDomainRead | Self::InstanceDomainUpdate => return None,
            Self::EnvironmentLogsRead => K::Environment,
            Self::WebhookList | Self::WebhookCreate => K::Project,
            Self::WebhookTest | Self::WebhookDelete => K::Webhook,
            Self::MemberList | Self::MemberInvite | Self::MemberLeave => K::Workspace,
            Self::InviteRevoke => K::Invite,
            Self::MemberUpdate | Self::MemberRemove => K::Member,
            Self::DeploymentExec => K::Deployment,
            Self::RunEnvRead => K::ServiceInstance,
            Self::CronRunsRead | Self::CronRunCreate => K::ServiceInstance,
            Self::NotificationChannelList
            | Self::NotificationChannelCreate
            | Self::AlertRuleList
            | Self::AlertRuleCreate
            | Self::AlertHistoryRead => K::Workspace,
            Self::NotificationChannelTest | Self::NotificationChannelDelete => {
                K::NotificationChannel
            }
            Self::AlertRuleUpdate | Self::AlertRuleDelete => K::AlertRule,
            Self::ProjectAlertRuleList => K::Project,
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
            | Self::SharedVariablesRead
            | Self::DeploymentRead => Role::Viewer,
            Self::VariablesWrite
            | Self::SharedVariablesWrite
            | Self::EnvironmentDeploy
            | Self::DeploymentCreate
            | Self::DeploymentRemove
            | Self::DeploymentRollout => Role::Deployer,
            Self::ProjectCreate
            | Self::ProjectUpdate
            | Self::EnvironmentCreate
            | Self::EnvironmentUpdate
            | Self::EnvironmentSync
            | Self::ServiceCreate
            | Self::ServiceUpdate
            | Self::ServiceDelete
            | Self::InstanceDelete
            | Self::InstanceUpdate
            | Self::VariableReveal
            | Self::EnvironmentConfigRead
            | Self::EnvironmentDiscard
            | Self::DomainCreate
            | Self::DomainUpdate
            | Self::DomainDelete
            | Self::VolumeCreate
            | Self::TcpProxyCreate
            | Self::TcpProxyDelete
            | Self::BucketProvision
            | Self::BucketUnlink
            | Self::GithubRepoList => Role::Member,
            Self::WorkspaceUpdate
            | Self::WorkspaceDelete
            | Self::ProjectDelete
            | Self::EnvironmentDelete
            | Self::VolumeDelete
            | Self::BucketDelete
            | Self::GithubInstall => Role::Admin,
            Self::SessionRevoke
            | Self::NodeList
            | Self::NodeJoinTokenCreate
            | Self::NodeRevoke
            | Self::NodeUpdate
            | Self::NodeRead
            | Self::NodeResizeOptions
            | Self::NodeResize
            | Self::GithubAppCreate
            | Self::GithubAppDelete => Role::Admin,
            Self::NodeMigrationRead | Self::NodeMigrate | Self::ServerMigrationAbort => Role::Admin,
            Self::ServerMigrationRead | Self::InstanceMoveRead => Role::Viewer,
            // Like the move without data (services::move_instance).
            Self::InstanceMove | Self::InstanceMoveAbort => Self::InstanceUpdate.min_role(),
            Self::BackupRead => Role::Viewer,
            Self::BackupCreate => Role::Deployer,
            Self::BackupTargetList
            | Self::BackupScheduleUpdate
            | Self::BackupRestore
            | Self::BackupDelete => Role::Member,
            Self::BackupTargetCreate
            | Self::BackupTargetDelete
            | Self::SelfBackupRead
            | Self::SelfBackupUpdate => Role::Admin,
            Self::UpdatesRead | Self::UpdatesUpdate | Self::UpdatesRun => Role::Admin,
            Self::PrivateAccessRead | Self::PrivateAccessUpdate => Role::Admin,
            // Account-owned: the role is not consulted.
            Self::TokenRevoke | Self::PasskeyRename | Self::PasskeyDelete => Role::Admin,
            Self::TunnelOpen => Role::Member,
            Self::CloudflareRead => Role::Member,
            Self::CloudflareConnect | Self::CloudflareDisconnect => Role::Admin,
            // Rows hold application data and credentials grant full access, so
            // even reading is Member+ (Viewers and Deployers see neither).
            // Changing data bypasses the app's own rules: Admin+ (and sudo).
            Self::DatabaseRead | Self::DatabaseCredentials | Self::DatabasePassword => Role::Member,
            Self::DatabaseWrite => Role::Admin,
            Self::PitrRestore | Self::ReplicaCreate | Self::ReplicaPromote => Role::Member,
            Self::HetznerRead | Self::ServerList | Self::ServerRead => Role::Viewer,
            Self::ServerSuggest => Role::Member,
            Self::HetznerConnect
            | Self::HetznerDisconnect
            | Self::HetznerCatalog
            | Self::ServerProvision
            | Self::ServerDelete => Role::Admin,
            Self::InstanceDomainRead | Self::InstanceDomainUpdate => Role::Admin,
            // Same audience as a deployment's logs.
            Self::EnvironmentLogsRead => Role::Viewer,
            Self::WebhookList | Self::WebhookCreate | Self::WebhookTest | Self::WebhookDelete => {
                Role::Member
            }
            Self::MemberList | Self::MemberLeave => Role::Viewer,
            Self::MemberInvite | Self::InviteRevoke | Self::MemberUpdate | Self::MemberRemove => {
                Role::Admin
            }
            // A shell sees every secret the container has.
            Self::DeploymentExec | Self::RunEnvRead => Role::Member,
            // Like deployments: Viewers see runs, Deployers start them.
            Self::CronRunsRead => Role::Viewer,
            Self::CronRunCreate => Role::Deployer,
            // Like project webhooks: channels point at third parties.
            Self::NotificationChannelList
            | Self::NotificationChannelCreate
            | Self::NotificationChannelTest
            | Self::NotificationChannelDelete
            | Self::AlertRuleList
            | Self::AlertRuleCreate
            | Self::AlertRuleUpdate
            | Self::AlertRuleDelete
            | Self::AlertHistoryRead
            | Self::ProjectAlertRuleList => Role::Member,
        }
    }

    /// Destructive or secret-revealing actions need a recent re-authentication.
    pub fn requires_sudo(self) -> bool {
        match self {
            Self::WorkspaceDelete
            | Self::ProjectDelete
            | Self::EnvironmentDelete
            | Self::ServiceDelete
            | Self::InstanceDelete
            | Self::VolumeDelete
            | Self::BucketDelete
            | Self::VariableReveal
            | Self::NodeJoinTokenCreate
            | Self::NodeRevoke
            | Self::NodeResize
            | Self::GithubAppDelete => true,
            // Connecting GitHub happens on GitHub (the App's owner approves
            // there), so no recent sign-in here.
            Self::GithubAppCreate => false,
            // Spends money and takes services down for a while.
            Self::NodeMigrate | Self::ServerMigrationAbort => true,
            Self::NodeMigrationRead
            | Self::ServerMigrationRead
            | Self::InstanceMove
            | Self::InstanceMoveRead
            | Self::InstanceMoveAbort => false,
            Self::WorkspaceRead
            | Self::WorkspaceUpdate
            | Self::SessionRevoke
            | Self::ProjectCreate
            | Self::ProjectRead
            | Self::ProjectUpdate
            | Self::EnvironmentCreate
            | Self::EnvironmentUpdate
            | Self::EnvironmentSync
            | Self::ServiceCreate
            | Self::ServiceUpdate
            | Self::InstanceRead
            | Self::InstanceUpdate
            | Self::VariablesRead
            | Self::VariablesWrite
            | Self::SharedVariablesRead
            | Self::SharedVariablesWrite
            // API tokens (never sudo) must be able to pull and plan.
            | Self::EnvironmentConfigRead
            | Self::EnvironmentDeploy
            | Self::EnvironmentDiscard
            | Self::DeploymentCreate
            | Self::DeploymentRead
            | Self::DeploymentRemove
            | Self::DeploymentRollout
            | Self::DomainCreate
            | Self::DomainUpdate
            | Self::DomainDelete
            | Self::VolumeCreate
            | Self::TcpProxyCreate
            | Self::TcpProxyDelete
            | Self::BucketProvision
            | Self::BucketUnlink
            | Self::NodeList
            | Self::NodeUpdate
            | Self::NodeRead
            | Self::NodeResizeOptions
            | Self::GithubInstall
            | Self::GithubRepoList => false,
            Self::BackupTargetDelete
            | Self::BackupRestore
            | Self::BackupDelete
            | Self::SelfBackupUpdate => true,
            Self::UpdatesUpdate => true,
            // Sudo for dashboard sessions only: see `session_sudo`.
            Self::UpdatesRun => false,
            Self::UpdatesRead => false,
            Self::PrivateAccessUpdate => true,
            Self::PrivateAccessRead => false,
            Self::BackupTargetList
            | Self::BackupTargetCreate
            | Self::BackupRead
            | Self::BackupScheduleUpdate
            | Self::BackupCreate
            | Self::SelfBackupRead => false,
            Self::TokenRevoke | Self::TunnelOpen => false,
            // Removing a second factor weakens the account.
            Self::PasskeyRename => false,
            Self::PasskeyDelete => true,
            Self::CloudflareRead => false,
            Self::CloudflareConnect | Self::CloudflareDisconnect => true,
            // Credentials: API tokens (never sudo) need them for `modula connect`;
            // every read is audited instead.
            Self::DatabaseRead | Self::DatabaseCredentials => false,
            Self::DatabaseWrite | Self::DatabasePassword => true,
            // Both replace what the database serves.
            Self::PitrRestore | Self::ReplicaPromote => true,
            Self::ReplicaCreate => false,
            Self::HetznerRead
            | Self::HetznerCatalog
            | Self::ServerList
            | Self::ServerRead
            | Self::ServerSuggest => false,
            Self::HetznerConnect
            | Self::HetznerDisconnect
            | Self::ServerProvision
            | Self::ServerDelete => true,
            // Not destructive and reveals nothing; the CLI's API tokens set it.
            Self::InstanceDomainUpdate
            | Self::InstanceDomainRead
            | Self::EnvironmentLogsRead
            | Self::WebhookList
            | Self::WebhookCreate
            | Self::WebhookTest
            | Self::WebhookDelete
            | Self::NotificationChannelList
            | Self::NotificationChannelCreate
            | Self::NotificationChannelTest
            | Self::NotificationChannelDelete
            | Self::AlertRuleList
            | Self::AlertRuleCreate
            | Self::AlertRuleUpdate
            | Self::AlertRuleDelete
            | Self::AlertHistoryRead
            | Self::ProjectAlertRuleList => false,
            Self::MemberUpdate | Self::MemberRemove => true,
            Self::MemberList | Self::MemberInvite | Self::InviteRevoke | Self::MemberLeave => false,
            // Sudo for dashboard sessions only: see `session_sudo`.
            Self::DeploymentExec | Self::RunEnvRead => false,
            // `modula cron run` works with API tokens (never sudo).
            Self::CronRunsRead | Self::CronRunCreate => false,
        }
    }

    /// Actions the CLI needs that a cookie session must be recently
    /// re-authenticated for, while API tokens (which never have sudo) pass
    /// and every use is audited instead: revealing secrets, and starting an
    /// upgrade (`modula server update`; only signed releases install).
    pub fn session_sudo(self) -> bool {
        matches!(
            self,
            Self::DeploymentExec | Self::RunEnvRead | Self::UpdatesRun
        )
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
    Volume,
    TcpProxy,
    BackupTarget,
    Backup,
    ApiToken,
    Passkey,
    ProvisionedServer,
    Webhook,
    /// One workspace membership (`workspace_members.id`).
    Member,
    Invite,
    NotificationChannel,
    AlertRule,
    ServerMigration,
    InstanceMove,
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
        ResourceKind::Volume,
        ResourceKind::TcpProxy,
        ResourceKind::BackupTarget,
        ResourceKind::Backup,
        ResourceKind::ApiToken,
        ResourceKind::Passkey,
        ResourceKind::ProvisionedServer,
        ResourceKind::Webhook,
        ResourceKind::Member,
        ResourceKind::Invite,
        ResourceKind::NotificationChannel,
        ResourceKind::AlertRule,
        ResourceKind::ServerMigration,
        ResourceKind::InstanceMove,
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
            Self::Volume => "volume_id",
            Self::TcpProxy => "tcp_proxy_id",
            Self::BackupTarget => "backup_target_id",
            Self::Backup => "backup_id",
            Self::ApiToken => "token_id",
            Self::Passkey => "passkey_id",
            Self::ProvisionedServer => "server_id",
            Self::Webhook => "webhook_id",
            Self::Member => "member_id",
            Self::Invite => "invite_id",
            Self::NotificationChannel => "notification_channel_id",
            Self::AlertRule => "alert_rule_id",
            Self::ServerMigration => "migration_id",
            Self::InstanceMove => "move_id",
        }
    }

    pub fn owner(self) -> OwnerKind {
        match self {
            Self::Session | Self::ApiToken | Self::Passkey => OwnerKind::Account,
            Self::Node => OwnerKind::Instance,
            Self::Workspace
            | Self::Project
            | Self::Environment
            | Self::Service
            | Self::ServiceInstance
            | Self::Variable
            | Self::Deployment
            | Self::Domain
            | Self::Volume
            | Self::TcpProxy
            | Self::BackupTarget
            | Self::Backup
            | Self::ProvisionedServer
            | Self::Webhook
            | Self::Member
            | Self::Invite
            | Self::NotificationChannel
            | Self::AlertRule
            | Self::ServerMigration
            | Self::InstanceMove => OwnerKind::Workspace,
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
                 JOIN environments e ON e.id = v.environment_id
                 JOIN projects p ON p.id = e.project_id WHERE v.id = $1"
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
            Self::Volume => {
                "SELECT p.workspace_id FROM volumes v
                 JOIN service_instances i ON i.id = v.service_instance_id
                 JOIN services s ON s.id = i.service_id
                 JOIN projects p ON p.id = s.project_id WHERE v.id = $1"
            }
            Self::TcpProxy => {
                "SELECT p.workspace_id FROM tcp_proxies t
                 JOIN service_instances i ON i.id = t.service_instance_id
                 JOIN services s ON s.id = i.service_id
                 JOIN projects p ON p.id = s.project_id WHERE t.id = $1"
            }
            Self::BackupTarget => "SELECT workspace_id FROM backup_targets WHERE id = $1",
            // Control plane backups have no service instance: never reachable here.
            Self::Backup => {
                "SELECT p.workspace_id FROM backups b
                 JOIN service_instances i ON i.id = b.service_instance_id
                 JOIN services s ON s.id = i.service_id
                 JOIN projects p ON p.id = s.project_id WHERE b.id = $1"
            }
            Self::ApiToken => "SELECT user_id FROM api_tokens WHERE id = $1",
            Self::Passkey => "SELECT user_id FROM passkeys WHERE id = $1",
            Self::ProvisionedServer => "SELECT workspace_id FROM provisioned_servers WHERE id = $1",
            Self::Webhook => {
                "SELECT p.workspace_id FROM webhooks w
                 JOIN projects p ON p.id = w.project_id WHERE w.id = $1"
            }
            Self::Member => "SELECT workspace_id FROM workspace_members WHERE id = $1",
            Self::Invite => "SELECT workspace_id FROM workspace_invites WHERE id = $1",
            Self::NotificationChannel => {
                "SELECT workspace_id FROM notification_channels WHERE id = $1"
            }
            Self::AlertRule => "SELECT workspace_id FROM alert_rules WHERE id = $1",
            Self::ServerMigration => "SELECT workspace_id FROM server_migrations WHERE id = $1",
            Self::InstanceMove => "SELECT workspace_id FROM instance_moves WHERE id = $1",
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
        OwnerKind::Instance if actor.is_instance_admin && actor.workspace_scope().is_none() => {
            Scope::Instance
        }
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
            // A workspace-restricted API token sees nothing outside its workspace.
            if !actor.can_reach_workspace(owner) {
                return Err(AppError::NotFound);
            }
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
    fn database_writes_and_credentials_need_more() {
        assert_eq!(Action::DatabaseRead.min_role(), Role::Member);
        assert!(!Action::DatabaseRead.requires_sudo());
        assert_eq!(Action::DatabaseWrite.min_role(), Role::Admin);
        assert!(Action::DatabaseWrite.requires_sudo());
        assert_eq!(Action::DatabaseCredentials.min_role(), Role::Member);
        assert!(!Action::DatabaseCredentials.requires_sudo());
    }

    #[test]
    fn shells_and_run_env_are_member_with_session_sudo() {
        for a in [Action::DeploymentExec, Action::RunEnvRead] {
            assert_eq!(a.min_role(), Role::Member);
            assert!(a.session_sudo());
            assert!(!a.requires_sudo());
        }
        assert_eq!(
            Action::DeploymentExec.kind(),
            Some(ResourceKind::Deployment)
        );
        assert_eq!(
            Action::RunEnvRead.kind(),
            Some(ResourceKind::ServiceInstance)
        );
        assert!(!Action::TunnelOpen.session_sudo());
        // `modula server update`: the dashboard asks for a recent sign-in.
        assert!(Action::UpdatesRun.session_sudo() && !Action::UpdatesRun.requires_sudo());
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
            Action::GithubAppDelete,
            Action::UpdatesRead,
            Action::UpdatesUpdate,
            Action::UpdatesRun,
            Action::PrivateAccessRead,
            Action::PrivateAccessUpdate,
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
