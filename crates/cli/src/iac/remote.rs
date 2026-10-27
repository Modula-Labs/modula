//! What the server has: `GET /api/environments/{id}/config`, one read for
//! a whole environment (settings, domains, volume, TCP proxy, variables with
//! the values of non-sealed ones). The server audits each read.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::Api;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEnv {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub services: Vec<RemoteService>,
    #[serde(default)]
    pub shared: Vec<RemoteVar>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteService {
    pub service_id: String,
    pub service_name: String,
    pub instance_id: String,
    #[serde(default)]
    pub template: Option<String>,
    /// The template's own settings (what a template service starts from).
    #[serde(default)]
    pub template_config: Option<Value>,
    /// Variables the template created; managed by the template.
    #[serde(default)]
    pub template_variables: Vec<String>,
    pub config: Value,
    #[serde(default)]
    pub volume: Option<RemoteVolume>,
    #[serde(default)]
    pub tcp_proxy: Option<RemoteTcpProxy>,
    #[serde(default)]
    pub domains: Vec<RemoteDomain>,
    #[serde(default)]
    pub variables: Vec<RemoteVar>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteVar {
    pub id: String,
    pub name: String,
    pub sealed: bool,
    /// None for sealed variables.
    #[serde(default)]
    pub value: Option<String>,
}

// Values are secrets: never in debug output.
impl std::fmt::Debug for RemoteVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteVar")
            .field("name", &self.name)
            .field("sealed", &self.sealed)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDomain {
    pub id: String,
    pub hostname: String,
    /// `generated` or `custom`.
    pub kind: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub target_port: Option<u16>,
    #[serde(default)]
    pub rate_limit_per_minute: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteVolume {
    pub id: String,
    pub mount_path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTcpProxy {
    pub id: String,
    #[serde(default)]
    pub public_port: u16,
    pub target_port: u16,
}

impl RemoteEnv {
    pub async fn fetch(api: &Api, environment: &str) -> anyhow::Result<Self> {
        api.get(&format!("/api/environments/{environment}/config"))
            .await
    }

    pub fn service(&self, name: &str) -> Option<&RemoteService> {
        self.services.iter().find(|s| s.service_name == name)
    }
}
