pub mod account;
pub mod activity;
pub mod auth;
pub mod backups;
pub mod database;
pub mod deployments;
pub mod domains;
pub mod env_config;
pub mod github;
pub mod install;
pub mod instance;
pub mod nodes;
pub mod projects;
pub mod services;
pub mod storage;
pub mod tokens;
pub mod uploads;
pub mod variables;
pub mod workspaces;

use crate::authz::Action;
use crate::http::{Access, ApiRouter};
use crate::integrations::cloudflare::api as cloudflare;
use crate::integrations::hetzner::api as hetzner;
use crate::provision;
use crate::state::AppState;

/// Every API route, with its access policy. Keep this table readable: it is
/// the security review surface.
#[rustfmt::skip]
pub fn routes(state: AppState) -> ApiRouter {
    use Access::{Authenticated, Instance, MfaPending, Public, Resource, Sudo};
    ApiRouter::new(state)
        .get("/api/health", Public, instance::health)
        .get("/api/instance", Public, instance::status)
        // Installer and release files for servers joining this instance
        .get("/api/install.sh", Public, install::install_script)
        .get("/api/releases", Public, install::release_file)
        // Auth
        .post("/api/auth/setup", Public, auth::setup)
        .post("/api/auth/login", Public, auth::login)
        .post("/api/auth/mfa", MfaPending, auth::mfa_verify)
        .post("/api/auth/sudo", Authenticated, auth::sudo)
        .post("/api/auth/logout", Authenticated, auth::logout)
        .get("/api/auth/me", Authenticated, auth::me)
        // Own account
        .get("/api/account/security", Authenticated, account::security)
        .post("/api/account/totp", Sudo, account::totp_begin)
        .post("/api/account/totp/confirm", Sudo, account::totp_confirm)
        .delete("/api/account/totp", Sudo, account::totp_disable)
        .post("/api/account/backup-codes", Sudo, account::backup_codes_regenerate)
        .post("/api/account/password", Sudo, account::change_password)
        .get("/api/account/sessions", Authenticated, account::sessions)
        .delete("/api/account/sessions/{session_id}", Resource(Action::SessionRevoke), account::session_revoke)
        // API tokens (Authorization: Bearer mdt_…); creating one needs sudo, so tokens can't mint tokens.
        .get("/api/account/tokens", Authenticated, tokens::list)
        .post("/api/account/tokens", Sudo, tokens::create)
        .delete("/api/account/tokens/{token_id}", Resource(Action::TokenRevoke), tokens::revoke)
        // Workspaces
        .get("/api/workspaces", Authenticated, workspaces::list)
        .post("/api/workspaces", Authenticated, workspaces::create)
        .get("/api/workspaces/{workspace_id}", Resource(Action::WorkspaceRead), workspaces::get)
        .patch("/api/workspaces/{workspace_id}", Resource(Action::WorkspaceUpdate), workspaces::update)
        .delete("/api/workspaces/{workspace_id}", Resource(Action::WorkspaceDelete), workspaces::delete)
        // Projects and environments
        .get("/api/workspaces/{workspace_id}/projects", Resource(Action::WorkspaceRead), projects::list)
        .post("/api/workspaces/{workspace_id}/projects", Resource(Action::ProjectCreate), projects::create)
        .get("/api/projects/{project_id}", Resource(Action::ProjectRead), projects::get)
        .patch("/api/projects/{project_id}", Resource(Action::ProjectUpdate), projects::update)
        .delete("/api/projects/{project_id}", Resource(Action::ProjectDelete), projects::delete)
        .post("/api/projects/{project_id}/environments", Resource(Action::EnvironmentCreate), projects::create_environment)
        .delete("/api/environments/{environment_id}", Resource(Action::EnvironmentDelete), projects::delete_environment)
        .post("/api/environments/{environment_id}/sync", Resource(Action::EnvironmentSync), projects::sync_environment)
        .get("/api/projects/{project_id}/activity", Resource(Action::ProjectRead), activity::list)
        // Config as code: the whole environment, with non-sealed values (audited).
        .get("/api/environments/{environment_id}/config", Resource(Action::EnvironmentConfigRead), env_config::get)
        .get("/api/environments/{environment_id}/pending", Resource(Action::SharedVariablesRead), activity::pending)
        .post("/api/environments/{environment_id}/deploy", Resource(Action::EnvironmentDeploy), activity::deploy)
        // Services, per-environment config, variables
        .post("/api/projects/{project_id}/services", Resource(Action::ServiceCreate), services::create)
        .delete("/api/services/{service_id}", Resource(Action::ServiceDelete), services::delete)
        .get("/api/service-instances/{instance_id}", Resource(Action::InstanceRead), services::get_instance)
        .patch("/api/service-instances/{instance_id}", Resource(Action::InstanceUpdate), services::update_instance)
        .get("/api/service-instances/{instance_id}/variables", Resource(Action::VariablesRead), variables::instance_variables)
        .patch("/api/service-instances/{instance_id}/variables", Resource(Action::VariablesWrite), variables::update_instance_variables)
        .get("/api/environments/{environment_id}/variables", Resource(Action::SharedVariablesRead), variables::shared_variables)
        .patch("/api/environments/{environment_id}/variables", Resource(Action::SharedVariablesWrite), variables::update_shared_variables)
        .get("/api/environments/{environment_id}/variable-names", Resource(Action::SharedVariablesRead), variables::names)
        .post("/api/variables/{variable_id}/reveal", Resource(Action::VariableReveal), variables::reveal)
        // Deployments
        .post("/api/service-instances/{instance_id}/deployments", Resource(Action::DeploymentCreate), deployments::create)
        .get("/api/service-instances/{instance_id}/deployments", Resource(Action::InstanceRead), deployments::list)
        .get("/api/deployments/{deployment_id}", Resource(Action::DeploymentRead), deployments::get)
        .delete("/api/deployments/{deployment_id}", Resource(Action::DeploymentRemove), deployments::remove)
        .get("/api/deployments/{deployment_id}/logs", Resource(Action::DeploymentRead), deployments::logs)
        .post("/api/deployments/{deployment_id}/redeploy", Resource(Action::DeploymentRollout), deployments::redeploy)
        .post("/api/deployments/{deployment_id}/rollback", Resource(Action::DeploymentRollout), deployments::rollback)
        .post("/api/deployments/{deployment_id}/restart", Resource(Action::DeploymentRollout), deployments::restart)
        // Source uploads (`modula-deploy up`): gzipped tarball in, deployed with
        // POST …/deployments {source: {upload}}. Never downloadable by clients.
        .post("/api/service-instances/{instance_id}/uploads", Resource(Action::DeploymentCreate), uploads::create)
        // Nodes fetch an upload they are building; authorized by a signed,
        // deployment-bound token from their desired state (see deploy::upload).
        .get(crate::deploy::upload::DOWNLOAD_PATH, Public, uploads::download)
        // Domains
        .get("/api/service-instances/{instance_id}/domains", Resource(Action::InstanceRead), domains::list)
        .post("/api/service-instances/{instance_id}/domains", Resource(Action::DomainCreate), domains::generate)
        .post("/api/service-instances/{instance_id}/custom-domains", Resource(Action::DomainCreate), domains::create_custom)
        .post("/api/domains/{domain_id}/check", Resource(Action::DomainUpdate), domains::check)
        .patch("/api/domains/{domain_id}", Resource(Action::DomainUpdate), domains::update)
        .delete("/api/domains/{domain_id}", Resource(Action::DomainDelete), domains::delete)
        // Databases: templates, volumes, TCP proxies
        .get("/api/templates", Authenticated, storage::templates)
        .post("/api/service-instances/{instance_id}/volume", Resource(Action::VolumeCreate), storage::create_volume)
        .delete("/api/volumes/{volume_id}", Resource(Action::VolumeDelete), storage::delete_volume)
        .post("/api/service-instances/{instance_id}/tcp-proxy", Resource(Action::TcpProxyCreate), storage::create_tcp_proxy)
        .delete("/api/tcp-proxies/{tcp_proxy_id}", Resource(Action::TcpProxyDelete), storage::delete_tcp_proxy)
        // Private TCP tunnel to the running container (WebSocket; `modula-deploy connect`)
        .get("/api/service-instances/{instance_id}/tunnel", Resource(Action::TunnelOpen), crate::tunnel::open)
        // Backups: storage targets per workspace, schedules and backups per database
        .get("/api/workspaces/{workspace_id}/backup-targets", Resource(Action::BackupTargetList), backups::list_targets)
        .post("/api/workspaces/{workspace_id}/backup-targets", Resource(Action::BackupTargetCreate), backups::create_target)
        .delete("/api/backup-targets/{backup_target_id}", Resource(Action::BackupTargetDelete), backups::delete_target)
        .get("/api/service-instances/{instance_id}/backups", Resource(Action::BackupRead), backups::list)
        .post("/api/service-instances/{instance_id}/backups", Resource(Action::BackupCreate), backups::create)
        .put("/api/service-instances/{instance_id}/backup-schedule", Resource(Action::BackupScheduleUpdate), backups::put_schedule)
        .post("/api/backups/{backup_id}/restore", Resource(Action::BackupRestore), backups::restore)
        .delete("/api/backups/{backup_id}", Resource(Action::BackupDelete), backups::delete)
        .get("/api/instance/self-backup", Instance(Action::SelfBackupRead), backups::get_self_backup)
        .put("/api/instance/self-backup", Instance(Action::SelfBackupUpdate), backups::put_self_backup)
        // Cloudflare integration (token is write-only; see integrations/cloudflare)
        .get("/api/workspaces/{workspace_id}/integrations/cloudflare", Resource(Action::CloudflareRead), cloudflare::status)
        .post("/api/workspaces/{workspace_id}/integrations/cloudflare/verify", Resource(Action::CloudflareConnect), cloudflare::verify)
        .put("/api/workspaces/{workspace_id}/integrations/cloudflare", Resource(Action::CloudflareConnect), cloudflare::connect)
        .delete("/api/workspaces/{workspace_id}/integrations/cloudflare", Resource(Action::CloudflareDisconnect), cloudflare::disconnect)
        .get("/api/workspaces/{workspace_id}/integrations/cloudflare/zones", Resource(Action::CloudflareRead), cloudflare::zones)
        .get("/api/workspaces/{workspace_id}/integrations/cloudflare/r2", Resource(Action::CloudflareRead), cloudflare::r2_info)
        .post("/api/workspaces/{workspace_id}/integrations/cloudflare/r2-targets", Resource(Action::BackupTargetCreate), cloudflare::create_r2_target)
        // Read through DomainUpdate (Member): only members can act on the answer.
        .get("/api/domains/{domain_id}/ingress", Resource(Action::DomainUpdate), cloudflare::ingress)
        .put("/api/domains/{domain_id}/ingress", Resource(Action::DomainUpdate), cloudflare::set_ingress)
        // Database tab (Postgres services; run by the node, see api/database.rs)
        .get("/api/service-instances/{instance_id}/database/tables", Resource(Action::DatabaseRead), database::tables)
        .get("/api/service-instances/{instance_id}/database/rows", Resource(Action::DatabaseRead), database::rows)
        .post("/api/service-instances/{instance_id}/database/rows", Resource(Action::DatabaseWrite), database::insert_row)
        .patch("/api/service-instances/{instance_id}/database/rows", Resource(Action::DatabaseWrite), database::update_row)
        .delete("/api/service-instances/{instance_id}/database/rows", Resource(Action::DatabaseWrite), database::delete_row)
        // Read-only queries need DatabaseRead; the handler requires DatabaseWrite otherwise.
        .post("/api/service-instances/{instance_id}/database/query", Resource(Action::DatabaseRead), database::query)
        .get("/api/service-instances/{instance_id}/database/stats", Resource(Action::DatabaseRead), database::stats)
        .post("/api/service-instances/{instance_id}/database/vacuum", Resource(Action::DatabaseWrite), database::vacuum)
        .get("/api/service-instances/{instance_id}/database/extensions", Resource(Action::DatabaseRead), database::extensions)
        .post("/api/service-instances/{instance_id}/database/extensions", Resource(Action::DatabaseWrite), database::extension)
        .get("/api/service-instances/{instance_id}/database/credentials", Resource(Action::DatabaseRead), database::credentials)
        .post("/api/service-instances/{instance_id}/database/password", Resource(Action::DatabasePassword), database::regenerate_password)
        // Servers (instance admins)
        .get("/api/nodes", Instance(Action::NodeList), nodes::list)
        .post("/api/nodes/join-tokens", Instance(Action::NodeJoinTokenCreate), nodes::create_join_token)
        .patch("/api/nodes/{node_id}", Resource(Action::NodeUpdate), nodes::update)
        .delete("/api/nodes/{node_id}", Resource(Action::NodeRevoke), nodes::revoke)
        .get("/api/locations", Authenticated, nodes::locations)
        // Hetzner integration and servers created through it (token is write-only; see integrations/hetzner)
        .get("/api/workspaces/{workspace_id}/integrations/hetzner", Resource(Action::HetznerRead), hetzner::status)
        .put("/api/workspaces/{workspace_id}/integrations/hetzner", Resource(Action::HetznerConnect), hetzner::connect)
        .patch("/api/workspaces/{workspace_id}/integrations/hetzner", Resource(Action::HetznerConnect), hetzner::update_caps)
        .delete("/api/workspaces/{workspace_id}/integrations/hetzner", Resource(Action::HetznerDisconnect), hetzner::disconnect)
        .get("/api/workspaces/{workspace_id}/integrations/hetzner/catalog", Resource(Action::HetznerCatalog), hetzner::catalog)
        .get("/api/workspaces/{workspace_id}/integrations/hetzner/ssh-keys", Resource(Action::HetznerCatalog), hetzner::ssh_keys)
        .get("/api/workspaces/{workspace_id}/servers", Resource(Action::ServerList), provision::api::list)
        .post("/api/workspaces/{workspace_id}/servers/provision", Resource(Action::ServerProvision), provision::api::provision)
        .get("/api/workspaces/{workspace_id}/servers/suggest", Resource(Action::ServerSuggest), provision::suggest::suggest)
        .get("/api/servers/{server_id}", Resource(Action::ServerRead), provision::api::get)
        .delete("/api/servers/{server_id}", Resource(Action::ServerDelete), provision::api::delete)
        // GitHub App: one per instance (admins), installed per workspace.
        .get("/api/github/app", Authenticated, github::status)
        .post("/api/github/app/manifest", Instance(Action::GithubAppCreate), github::manifest)
        // Bound to the admin's session by the single-use state, so not Public.
        .get("/api/github/app/callback", Authenticated, github::app_callback)
        .post("/api/workspaces/{workspace_id}/github/install", Resource(Action::GithubInstall), github::install)
        .get("/api/github/install/callback", Authenticated, github::install_callback)
        .get("/api/workspaces/{workspace_id}/github/repos", Resource(Action::GithubRepoList), github::repos)
        // Authenticated by HMAC signature; the only path exempt from the Origin check.
        .post(github::WEBHOOK_PATH, Public, github::webhook)
}
