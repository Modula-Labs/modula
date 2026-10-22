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
use uuid::Uuid;

use crate::auth::session::{self, SessionKind};
use crate::authz::{self, Action, Resource, ResourceKind};
use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No authentication. Must not carry path parameters.
    Public,
    /// Any fully authenticated user. Must not carry path parameters (no resource
    /// to authorize against — use `Resource` for those).
    Authenticated,
    /// Authenticated + `authorize(action, <the single {x_id} param>)`.
    Resource(Action),
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
        Access::Public | Access::Authenticated if !params.is_empty() => {
            Err("routes with resource params must use Access::Resource".into())
        }
        // Exactly one id per path: nested ids would need a cross-check that the
        // child belongs to the parent, which is how IDORs happen. Use flat routes.
        Access::Resource(_) if params.len() != 1 => {
            Err("Access::Resource routes need exactly one {x_id} param".into())
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
    let presented = session::read_cookie(req.headers(), SessionKind::Full.cookie_name(&state.cfg))
        .ok_or(AppError::Unauthorized)?;
    let actor = session::resolve(&state.db, &presented, SessionKind::Full)
        .await?
        .ok_or(AppError::Unauthorized)?;

    let (mut parts, body) = req.into_parts();
    if let Access::Resource(action) = access {
        let params = RawPathParams::from_request_parts(&mut parts, state)
            .await
            .map_err(|_| AppError::NotFound)?;
        let (name, value) = params.iter().next().ok_or(AppError::NotFound)?;
        let kind = ResourceKind::from_param(name)
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("unmapped param {name}")))?;
        let id = Uuid::parse_str(value).map_err(|_| AppError::NotFound)?;
        let grant = authz::authorize(&state.db, &actor, action, Resource { kind, id }).await?;
        parts.extensions.insert(grant);
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
    }
}
