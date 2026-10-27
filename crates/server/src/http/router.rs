//! The only place routes are registered. Each route must declare an [`Access`]
//! policy; the policy is enforced by a route layer (not by remembering to add an
//! extractor), and the table of specs is checked by tests.

use axum::Router;
use axum::extract::{FromRequestParts, RawPathParams, Request, State};
use axum::handler::Handler;
use axum::http::Method;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, patch, post, put};
use modula_id::Id;

use crate::api::tokens;
use crate::auth::session::{self, SessionKind};
use crate::authz::{self, Action, Resource, ResourceKind};
use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No authentication. Must not carry path parameters.
    Public,
    /// Only a half-logged-in user completing a second factor (mfa cookie).
    MfaPending,
    /// Any fully authenticated user. Must not carry path parameters (no resource
    /// to authorize against — use `Resource` for those).
    Authenticated,
    /// Authenticated with a recent re-authentication ("sudo mode").
    Sudo,
    /// Authenticated + `authorize(action, <the single {x_id} param>)`, plus sudo
    /// when `action.requires_sudo()`.
    Resource(Action),
    /// Instance admins only, for instance-level actions without a path
    /// parameter (`action.kind() == None`), plus sudo when required.
    Instance(Action),
}

impl Access {
    pub fn requires_sudo(self) -> bool {
        match self {
            Self::Sudo => true,
            Self::Resource(a) | Self::Instance(a) => a.requires_sudo(),
            Self::Public | Self::MfaPending | Self::Authenticated => false,
        }
    }

    /// Sudo for cookie sessions only (see [`crate::authz::Action::session_sudo`]).
    pub fn session_sudo(self) -> bool {
        matches!(self, Self::Resource(a) | Self::Instance(a) if a.session_sudo())
    }
}

#[derive(Debug, Clone)]
pub struct RouteSpec {
    pub method: Method,
    pub path: &'static str,
    pub access: Access,
}

pub struct ApiRouter {
    state: AppState,
    router: Router<AppState>,
    specs: Vec<RouteSpec>,
}

macro_rules! method_fn {
    ($name:ident, $route:ident, $method:expr) => {
        pub fn $name<H, T>(self, path: &'static str, access: Access, handler: H) -> Self
        where
            H: Handler<T, AppState>,
            T: 'static,
        {
            self.add($method, path, access, $route(handler))
        }
    };
}

impl ApiRouter {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            router: Router::new(),
            specs: Vec::new(),
        }
    }

    method_fn!(get, get, Method::GET);
    method_fn!(post, post, Method::POST);
    method_fn!(put, put, Method::PUT);
    method_fn!(patch, patch, Method::PATCH);
    method_fn!(delete, delete, Method::DELETE);

    fn add(
        mut self,
        method: Method,
        path: &'static str,
        access: Access,
        mr: MethodRouter<AppState>,
    ) -> Self {
        if let Err(e) = validate(path, access) {
            panic!("invalid route {method} {path}: {e}");
        }
        if self
            .specs
            .iter()
            .any(|s| s.path == path && s.method == method)
        {
            panic!("duplicate route {method} {path}");
        }
        let mr = match access {
            Access::Public => mr,
            guarded => mr.route_layer(middleware::from_fn_with_state(
                self.state.clone(),
                move |st: State<AppState>, req: Request, next: Next| {
                    enforce(st, guarded, req, next)
                },
            )),
        };
        self.router = self.router.route(path, mr);
        self.specs.push(RouteSpec {
            method,
            path,
            access,
        });
        self
    }

    pub fn finish(self) -> (Router<AppState>, Vec<RouteSpec>) {
        (self.router, self.specs)
    }
}

pub fn path_params(path: &str) -> Vec<&str> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .collect()
}

fn validate(path: &str, access: Access) -> Result<(), String> {
    if !path.starts_with("/api/") {
        return Err("API routes must live under /api/".into());
    }
    let params = path_params(path);
    for p in &params {
        if ResourceKind::from_param(p).is_none() {
            return Err(format!(
                "unknown path param {{{p}}}; add it to authz::ResourceKind"
            ));
        }
    }
    match access {
        Access::Resource(action) => {
            // Exactly one id per path: nested ids would need a cross-check that the
            // child belongs to the parent, which is how IDORs happen. Use flat routes.
            let [param] = params[..] else {
                return Err("Access::Resource routes need exactly one {x_id} param".into());
            };
            let kind = ResourceKind::from_param(param).expect("checked above");
            if action.kind() != Some(kind) {
                return Err(format!("{action:?} does not apply to {kind:?}"));
            }
            Ok(())
        }
        Access::Instance(action) => {
            if action.kind().is_some() {
                return Err(format!(
                    "{action:?} is resource-scoped; use Access::Resource"
                ));
            }
            if !params.is_empty() {
                return Err("Access::Instance routes take no path params".into());
            }
            Ok(())
        }
        _ if !params.is_empty() => {
            Err("routes with resource params must use Access::Resource".into())
        }
        _ => Ok(()),
    }
}

async fn enforce(
    State(state): State<AppState>,
    access: Access,
    req: Request,
    next: Next,
) -> Response {
    match authorize_request(&state, access, req).await {
        Ok(req) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

async fn authorize_request(
    state: &AppState,
    access: Access,
    req: Request,
) -> Result<Request, AppError> {
    // An `Authorization` header means API-token auth only: never fall back to
    // cookies (the CSRF middleware let the request through on the token alone).
    let bearer = match req.extensions().get::<tokens::BearerActor>() {
        Some(b) => Some(b.0.clone()),
        None => match tokens::bearer(req.headers()) {
            Some(presented) => Some(
                tokens::resolve(&state.db, presented?)
                    .await?
                    .ok_or(AppError::Unauthorized)?,
            ),
            None => None,
        },
    };
    if access == Access::MfaPending && bearer.is_some() {
        return Err(AppError::Unauthorized);
    }
    if access == Access::MfaPending {
        let presented = session::read_cookie(
            req.headers(),
            SessionKind::MfaPending.cookie_name(&state.cfg),
        )
        .ok_or(AppError::Unauthorized)?;
        let actor = session::resolve(&state.db, &presented, SessionKind::MfaPending)
            .await?
            .ok_or(AppError::Unauthorized)?;
        let (mut parts, body) = req.into_parts();
        parts.extensions.insert(session::PendingActor(actor));
        return Ok(Request::from_parts(parts, body));
    }

    let actor = match bearer {
        Some(actor) => actor,
        None => {
            let presented =
                session::read_cookie(req.headers(), SessionKind::Full.cookie_name(&state.cfg))
                    .ok_or(AppError::Unauthorized)?;
            session::resolve(&state.db, &presented, SessionKind::Full)
                .await?
                .ok_or(AppError::Unauthorized)?
        }
    };

    // Instance administration is out of reach of workspace-restricted tokens.
    if matches!(access, Access::Instance(_))
        && (!actor.is_instance_admin || actor.workspace_scope().is_some())
    {
        return Err(AppError::Forbidden);
    }
    let (mut parts, body) = req.into_parts();
    if let Access::Resource(action) = access {
        let params = RawPathParams::from_request_parts(&mut parts, state)
            .await
            .map_err(|_| AppError::NotFound)?;
        let (name, value) = params.iter().next().ok_or(AppError::NotFound)?;
        let kind = ResourceKind::from_param(name)
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("unmapped param {name}")))?;
        let id = value.parse::<Id>().map_err(|_| AppError::NotFound)?;
        let grant = authz::authorize(&state.db, &actor, action, Resource { kind, id }).await?;
        parts.extensions.insert(grant);
    }
    // After authorization, so a non-member learns nothing (404) before sudo (403).
    let needs_sudo = access.requires_sudo() || (access.session_sudo() && actor.api_token.is_none());
    if needs_sudo && !actor.sudo_active() {
        return Err(AppError::SudoRequired);
    }
    parts.extensions.insert(actor);
    Ok(Request::from_parts(parts, body))
}

impl<S: Send + Sync> FromRequestParts<S> for authz::Grant {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<authz::Grant>()
            .copied()
            .ok_or_else(|| {
                AppError::Internal(anyhow::anyhow!(
                    "Grant extractor used on a non-resource route"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rules() {
        let a = Access::Resource(Action::WorkspaceRead);
        assert!(validate("/api/workspaces/{workspace_id}", a).is_ok());
        assert!(validate("/api/workspaces", Access::Authenticated).is_ok());
        assert!(validate("/workspaces", Access::Authenticated).is_err());
        assert!(validate("/api/workspaces/{workspace_id}", Access::Authenticated).is_err());
        assert!(validate("/api/workspaces/{workspace_id}", Access::Public).is_err());
        assert!(validate("/api/workspaces/{id}", a).is_err());
        assert!(validate("/api/workspaces", a).is_err());
        assert!(validate("/api/workspaces/{workspace_id}/x/{workspace_id}", a).is_err());
        // An account action on a workspace resource (and vice versa) is rejected.
        assert!(
            validate(
                "/api/workspaces/{workspace_id}",
                Access::Resource(Action::SessionRevoke)
            )
            .is_err()
        );
        assert!(
            validate(
                "/api/account/sessions/{session_id}",
                Access::Resource(Action::SessionRevoke)
            )
            .is_ok()
        );
        assert!(validate("/api/nodes", Access::Instance(Action::NodeList)).is_ok());
        assert!(validate("/api/nodes/{node_id}", Access::Instance(Action::NodeRevoke)).is_err());
        assert!(validate("/api/nodes", Access::Instance(Action::WorkspaceRead)).is_err());
        assert!(validate("/api/nodes/{node_id}", Access::Resource(Action::NodeRevoke)).is_ok());
        // A same-owner but different kind (deployment action on a project) is rejected.
        assert!(
            validate(
                "/api/projects/{project_id}",
                Access::Resource(Action::DeploymentRemove)
            )
            .is_err()
        );
        assert!(validate("/api/x/{session_id}", Access::Sudo).is_err());
        assert!(validate("/api/x/{session_id}", Access::MfaPending).is_err());
    }
}
