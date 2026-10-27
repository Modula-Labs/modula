//! Append-only audit trail for security-relevant events and all mutations.

use modula_id::Id;
use serde_json::Value;
use sqlx::PgExecutor;

use crate::auth::session::Actor;
use crate::error::AppResult;
use crate::http::meta::RequestMeta;

pub struct Entry<'a> {
    pub action: &'static str,
    pub workspace_id: Option<Id>,
    pub user_id: Option<Id>,
    pub session_id: Option<Id>,
    pub token_id: Option<Id>,
    pub target: Option<(&'static str, String)>,
    pub meta: &'a RequestMeta,
    pub metadata: Value,
}

impl<'a> Entry<'a> {
    pub fn new(action: &'static str, meta: &'a RequestMeta) -> Self {
        Self {
            action,
            workspace_id: None,
            user_id: None,
            session_id: None,
            token_id: None,
            target: None,
            meta,
            metadata: Value::Object(Default::default()),
        }
    }

    pub fn actor(mut self, actor: &Actor) -> Self {
        self.user_id = Some(actor.user_id);
        match actor.api_token {
            Some(t) => self.token_id = Some(t.id),
            None => self.session_id = Some(actor.session_id),
        }
        self
    }

    pub fn user(mut self, user_id: Id) -> Self {
        self.user_id = Some(user_id);
        self
    }

    pub fn workspace(mut self, id: Id) -> Self {
        self.workspace_id = Some(id);
        self
    }

    pub fn target(mut self, kind: &'static str, id: impl ToString) -> Self {
        self.target = Some((kind, id.to_string()));
        self
    }

    pub fn metadata(mut self, v: Value) -> Self {
        self.metadata = v;
        self
    }

    pub async fn record<'e>(self, exec: impl PgExecutor<'e>) -> AppResult<()> {
        let (target_type, target_id) = match self.target {
            Some((t, id)) => (Some(t), Some(id)),
            None => (None, None),
        };
        sqlx::query(
            "INSERT INTO audit_log
               (workspace_id, actor_user_id, actor_session_id, actor_token_id, action,
                target_type, target_id, ip, user_agent, metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(self.workspace_id)
        .bind(self.user_id)
        .bind(self.session_id)
        .bind(self.token_id)
        .bind(self.action)
        .bind(target_type)
        .bind(target_id)
        .bind(self.meta.ip_string())
        .bind(&self.meta.user_agent)
        .bind(&self.metadata)
        .execute(exec)
        .await?;
        Ok(())
    }
}
