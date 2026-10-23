pub mod account;
pub mod auth;
pub mod deployments;
pub mod domains;
pub mod instance;
pub mod nodes;
pub mod projects;
pub mod services;
pub mod workspaces;

use crate::authz::Action;
use crate::http::{Access, ApiRouter};
use crate::state::AppState;

/// Every API route, with its access policy. Keep this table readable: it is
/// the security review surface.
#[rustfmt::skip]
pub fn routes(state: AppState) -> ApiRouter {
    use Access::{Authenticated, Instance, MfaPending, Public, Resource, Sudo};
    ApiRouter::new(state)
        .get("/api/health", Public, instance::health)
        .get("/api/instance", Public, instance::status)
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
        // Services, per-environment config, variables
        .post("/api/projects/{project_id}/services", Resource(Action::ServiceCreate), services::create)
        .delete("/api/services/{service_id}", Resource(Action::ServiceDelete), services::delete)
        .get("/api/service-instances/{instance_id}", Resource(Action::InstanceRead), services::get_instance)
        .patch("/api/service-instances/{instance_id}", Resource(Action::InstanceUpdate), services::update_instance)
        .get("/api/service-instances/{instance_id}/variables", Resource(Action::VariablesRead), services::variables)
        .patch("/api/service-instances/{instance_id}/variables", Resource(Action::VariablesWrite), services::update_variables)
        .post("/api/variables/{variable_id}/reveal", Resource(Action::VariableReveal), services::reveal_variable)
        // Deployments
        .post("/api/service-instances/{instance_id}/deployments", Resource(Action::DeploymentCreate), deployments::create)
        .get("/api/service-instances/{instance_id}/deployments", Resource(Action::InstanceRead), deployments::list)
        .get("/api/deployments/{deployment_id}", Resource(Action::DeploymentRead), deployments::get)
        .delete("/api/deployments/{deployment_id}", Resource(Action::DeploymentRemove), deployments::remove)
        .get("/api/deployments/{deployment_id}/logs", Resource(Action::DeploymentRead), deployments::logs)
        // Domains
        .get("/api/service-instances/{instance_id}/domains", Resource(Action::InstanceRead), domains::list)
        .post("/api/service-instances/{instance_id}/domains", Resource(Action::DomainCreate), domains::generate)
        .delete("/api/domains/{domain_id}", Resource(Action::DomainDelete), domains::delete)
        // Servers (instance admins)
        .get("/api/nodes", Instance(Action::NodeList), nodes::list)
        .post("/api/nodes/join-tokens", Instance(Action::NodeJoinTokenCreate), nodes::create_join_token)
        .delete("/api/nodes/{node_id}", Resource(Action::NodeRevoke), nodes::revoke)
}
