//! One typed method per API route the CLI uses.

use serde_json::{Value, json};

use super::Api;
use super::types::*;

/// Whose variables: one service in one environment, or the environment's
/// shared ones (`${{shared.NAME}}`).
#[derive(Debug, Clone, Copy)]
pub enum VariableScope<'a> {
    Instance(&'a str),
    Shared(&'a str),
}

impl VariableScope<'_> {
    fn path(self) -> String {
        match self {
            Self::Instance(i) => format!("/api/service-instances/{i}/variables"),
            Self::Shared(e) => format!("/api/environments/{e}/variables"),
        }
    }
}

/// Query of `GET /api/deployments/{id}/logs`.
#[derive(Debug, Clone, Copy)]
pub struct LogOptions {
    pub tail: u32,
    pub follow: bool,
    pub build: bool,
}

impl Api {
    pub async fn me(&self) -> anyhow::Result<Me> {
        self.get("/api/auth/me").await
    }

    pub async fn projects(&self, workspace: &str) -> anyhow::Result<Vec<Project>> {
        self.get(&format!("/api/workspaces/{workspace}/projects"))
            .await
    }

    pub async fn project(&self, id: &str) -> anyhow::Result<ProjectDetail> {
        self.get(&format!("/api/projects/{id}")).await
    }

    pub async fn create_project(
        &self,
        workspace: &str,
        name: &str,
    ) -> anyhow::Result<ProjectDetail> {
        self.post(
            &format!("/api/workspaces/{workspace}/projects"),
            &json!({ "name": name }),
        )
        .await
    }

    pub async fn create_environment(
        &self,
        project: &str,
        req: &NewEnvironment,
    ) -> anyhow::Result<Environment> {
        self.post(&format!("/api/projects/{project}/environments"), req)
            .await
    }

    /// Returns `{ id, name, instances: [...] }`.
    pub async fn create_service(&self, project: &str, req: &NewService) -> anyhow::Result<Value> {
        self.post(&format!("/api/projects/{project}/services"), req)
            .await
    }

    pub async fn instance(&self, id: &str) -> anyhow::Result<InstanceDetail> {
        self.get(&format!("/api/service-instances/{id}")).await
    }

    // ------------------------------------------------------------ deployments

    pub async fn pending(&self, environment: &str) -> anyhow::Result<Pending> {
        self.get(&format!("/api/environments/{environment}/pending"))
            .await
    }

    /// Deploy the given services' current config, or (None) every service
    /// with pending changes.
    pub async fn deploy_environment(
        &self,
        environment: &str,
        services: Option<&[String]>,
    ) -> anyhow::Result<DeployedList> {
        let body = match services {
            Some(ids) => json!({ "serviceIds": ids }),
            None => json!({}),
        };
        self.post(&format!("/api/environments/{environment}/deploy"), &body)
            .await
    }

    pub async fn deploy_instance(&self, instance: &str) -> anyhow::Result<Deployment> {
        self.post_empty(&format!("/api/service-instances/{instance}/deployments"))
            .await
    }

    /// Newest first, at most 50.
    pub async fn deployments(&self, instance: &str) -> anyhow::Result<Vec<Deployment>> {
        self.get(&format!("/api/service-instances/{instance}/deployments"))
            .await
    }

    pub async fn deployment(&self, id: &str) -> anyhow::Result<Deployment> {
        self.get(&format!("/api/deployments/{id}")).await
    }

    /// `redeploy`, `rollback` or `restart`.
    pub async fn rollout(&self, deployment: &str, action: &str) -> anyhow::Result<Deployment> {
        self.post_empty(&format!("/api/deployments/{deployment}/{action}"))
            .await
    }

    pub async fn remove_deployment(&self, id: &str) -> anyhow::Result<Deployment> {
        self.delete(&format!("/api/deployments/{id}")).await
    }

    /// The log stream (Server-Sent Events: `log`, `error`, `end`).
    pub async fn logs(
        &self,
        deployment: &str,
        opts: LogOptions,
    ) -> anyhow::Result<reqwest::Response> {
        let kind = if opts.build { "build" } else { "runtime" };
        self.stream(&format!(
            "/api/deployments/{deployment}/logs?tail={}&follow={}&kind={kind}",
            opts.tail, opts.follow
        ))
        .await
    }

    // ------------------------------------------------------------ variables

    pub async fn variables(&self, scope: VariableScope<'_>) -> anyhow::Result<Vec<Variable>> {
        self.get(&scope.path()).await
    }

    pub async fn update_variables(
        &self,
        scope: VariableScope<'_>,
        changes: &VariableChanges,
    ) -> anyhow::Result<Vec<Variable>> {
        self.patch(&scope.path(), changes).await
    }

    // ------------------------------------------------------------ domains

    pub async fn domains(&self, instance: &str) -> anyhow::Result<Vec<Domain>> {
        self.get(&format!("/api/service-instances/{instance}/domains"))
            .await
    }

    pub async fn generate_domain(&self, instance: &str) -> anyhow::Result<Domain> {
        self.post_empty(&format!("/api/service-instances/{instance}/domains"))
            .await
    }

    pub async fn add_custom_domain(
        &self,
        instance: &str,
        hostname: &str,
        port: Option<u16>,
    ) -> anyhow::Result<Domain> {
        self.post(
            &format!("/api/service-instances/{instance}/custom-domains"),
            &json!({ "hostname": hostname, "targetPort": port }),
        )
        .await
    }

    /// Replaces both the target port and the rate limit.
    pub async fn update_domain(
        &self,
        id: &str,
        port: Option<u16>,
        rate_limit_per_minute: Option<u32>,
    ) -> anyhow::Result<Domain> {
        self.patch(
            &format!("/api/domains/{id}"),
            &json!({ "targetPort": port, "rateLimitPerMinute": rate_limit_per_minute }),
        )
        .await
    }

    pub async fn check_domain(&self, id: &str) -> anyhow::Result<Domain> {
        self.post_empty(&format!("/api/domains/{id}/check")).await
    }

    pub async fn delete_domain(&self, id: &str) -> anyhow::Result<Value> {
        self.delete(&format!("/api/domains/{id}")).await
    }

    // ------------------------------------------------------------ storage

    pub async fn create_volume(
        &self,
        instance: &str,
        mount_path: &str,
    ) -> anyhow::Result<InstanceDetail> {
        self.post(
            &format!("/api/service-instances/{instance}/volume"),
            &json!({ "mountPath": mount_path }),
        )
        .await
    }

    pub async fn delete_volume(&self, id: &str) -> anyhow::Result<Value> {
        self.delete(&format!("/api/volumes/{id}")).await
    }

    pub async fn create_tcp_proxy(
        &self,
        instance: &str,
        target_port: u16,
    ) -> anyhow::Result<InstanceDetail> {
        self.post(
            &format!("/api/service-instances/{instance}/tcp-proxy"),
            &json!({ "targetPort": target_port }),
        )
        .await
    }

    pub async fn delete_tcp_proxy(&self, id: &str) -> anyhow::Result<Value> {
        self.delete(&format!("/api/tcp-proxies/{id}")).await
    }

    // ------------------------------------------------------------ database

    /// `read_only` runs one statement in a READ ONLY transaction that is
    /// rolled back; anything else needs write access.
    pub async fn db_query(
        &self,
        instance: &str,
        sql: &str,
        read_only: bool,
    ) -> anyhow::Result<Value> {
        self.post(
            &format!("/api/service-instances/{instance}/database/query"),
            &json!({ "sql": sql, "readOnly": read_only }),
        )
        .await
    }

    pub async fn db_tables(&self, instance: &str) -> anyhow::Result<Value> {
        self.get(&format!(
            "/api/service-instances/{instance}/database/tables"
        ))
        .await
    }
}
