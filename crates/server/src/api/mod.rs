pub mod account;
pub mod auth;
pub mod instance;
pub mod workspaces;

use crate::authz::Action;
use crate::http::{Access, ApiRouter};
use crate::state::AppState;

/// Every API route, with its access policy. Keep this table readable: it is
/// the security review surface.
#[rustfmt::skip]
pub fn routes(state: AppState) -> ApiRouter {
    use Access::{Authenticated, MfaPending, Public, Resource, Sudo};
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
}
