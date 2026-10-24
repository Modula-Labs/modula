//! Request and response shapes. Responses the CLI prints with `--json` keep
//! every field the server sent (`rest`), so agents see the whole object.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Me {
    pub user: User,
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub email: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDetail {
    pub id: String,
    pub name: String,
    pub environments: Vec<Environment>,
    pub services: Vec<Service>,
    pub instances: Vec<Instance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub id: String,
    pub name: String,
    /// The project's first environment (production): never deleted.
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub id: String,
    pub name: String,
    /// The database template it was created from (`postgres`, `mysql`, …).
    pub template: Option<String>,
}

/// A service in one environment, as listed in the project detail.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub service_id: String,
    pub environment_id: String,
    #[serde(default)]
    pub latest_deployment_id: Option<String>,
    /// Raw status of the newest deployment (`queued` … `active`, `failed`).
    #[serde(default)]
    pub latest_deployment_status: Option<String>,
    /// The first verified public hostname.
    #[serde(default)]
    pub domain: Option<String>,
    /// `image` or `repo`.
    #[serde(default)]
    pub source_type: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
}

/// `GET /api/service-instances/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceDetail {
    pub id: String,
    pub service_name: String,
    pub environment_name: String,
    /// `online`, `deploying`, `crashed` or `offline`.
    pub status: String,
    pub config: Value,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub volume: Option<Volume>,
    #[serde(default)]
    pub tcp_proxy: Option<TcpProxy>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl InstanceDetail {
    /// The port the service listens on (`config.port`).
    pub fn port(&self) -> Option<u16> {
        self.config
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub id: String,
    pub mount_path: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TcpProxy {
    pub id: String,
    pub public_port: u16,
    pub target_port: u16,
    #[serde(default)]
    pub host: String,
}

/// `GET /api/service-instances/{id}/run-env`. Values are secrets: no `Debug`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEnv {
    pub variables: std::collections::BTreeMap<String, String>,
    /// Sealed variables (and those referencing one), never sent.
    #[serde(default)]
    pub withheld: Vec<String>,
    #[serde(default)]
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deployment {
    pub id: String,
    /// `queued`, `building`, `deploying`, `active`, `failed`, `crashed`,
    /// `removed` or `cancelled`.
    pub status: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub trigger: String,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub commit_message: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl Deployment {
    /// Whether the deployment has settled (it won't change on its own).
    pub fn is_final(&self) -> bool {
        is_final_status(&self.status)
    }
}

pub fn is_final_status(status: &str) -> bool {
    matches!(
        status,
        "active" | "failed" | "crashed" | "removed" | "cancelled"
    )
}

/// `POST /api/environments/{id}/deploy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deployed {
    pub service_id: String,
    pub deployment_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployedList {
    pub deployments: Vec<Deployed>,
}

/// `GET /api/environments/{id}/pending`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    pub count: usize,
    pub services: Vec<PendingService>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingService {
    pub service_id: String,
    pub service_name: String,
    pub instance_id: String,
}

/// A variable's metadata; values are never returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Variable {
    pub id: String,
    pub name: String,
    pub sealed: bool,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Domain {
    pub id: String,
    pub hostname: String,
    /// `generated` or `custom`.
    pub kind: String,
    /// `pending`, `verified` or `failed`.
    pub status: String,
    #[serde(default)]
    pub target_port: Option<u16>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub dns_records: Vec<DnsRecord>,
    #[serde(default)]
    pub provider_note: Option<String>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub value: String,
}

/// `GET /api/service-instances/{id}/database/credentials`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbCredentials {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// None for engines without one (Redis, MongoDB).
    #[serde(default)]
    pub database: Option<String>,
    pub private_url: Option<String>,
}

/// `POST …/database/query`: the last result set of the script.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    #[serde(default)]
    pub columns: Vec<Column>,
    /// Rows as arrays of (string or null) cells; objects are tolerated.
    #[serde(default)]
    pub rows: Vec<Value>,
    #[serde(default)]
    pub row_count: Option<u64>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<f64>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: String,
}

/// `GET …/database/tables`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tables {
    #[serde(default)]
    pub tables: Vec<Table>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Table {
    #[serde(default)]
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub row_estimate: Option<i64>,
}

// ------------------------------------------------------------------ requests

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewService {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewEnvironment {
    pub name: String,
    /// Environment id to duplicate (services, settings, all variables);
    /// none: an empty environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_from: Option<String>,
    /// Service id → branch, for repo services.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub branches: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct VariableChanges {
    pub set: std::collections::BTreeMap<String, String>,
    pub sealed: Vec<String>,
    pub delete: Vec<String>,
}

/// `POST /api/service-instances/{id}/uploads`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceUpload {
    pub id: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// A deployment of an upload (`up`).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadDeployment {
    pub source: UploadRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<LocalGit>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UploadRef {
    pub upload: String,
}

/// The checkout an upload was packed from (shown with the deployment).
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalGit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
}
