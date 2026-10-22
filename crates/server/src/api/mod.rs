pub mod auth;
pub mod instance;
pub mod workspaces;

use crate::authz::Action;
use crate::http::{Access, ApiRouter};
use crate::state::AppState;

/// Every API route, with its access policy. Keep this table readable: it is
/// the security review surface.
pub fn routes(state: AppState) -> ApiRouter {
    use Access::{Authenticated, Public, Resource};
    ApiRouter::new(state)
        .get("/api/health", Public, instance::health)
        .get("/api/instance", Public, instance::status)
        .post("/api/auth/setup", Public, auth::setup)
        .post("/api/auth/login", Public, auth::login)
        .post("/api/auth/logout", Authenticated, auth::logout)
        .get("/api/auth/me", Authenticated, auth::me)
        .get("/api/workspaces", Authenticated, workspaces::list)
        .post("/api/workspaces", Authenticated, workspaces::create)
        .get(
            "/api/workspaces/{workspace_id}",
            Resource(Action::WorkspaceRead),
            workspaces::get,
        )
        .patch(
            "/api/workspaces/{workspace_id}",
            Resource(Action::WorkspaceUpdate),
            workspaces::update,
        )
        .delete(
            "/api/workspaces/{workspace_id}",
            Resource(Action::WorkspaceDelete),
            workspaces::delete,
        )
}
