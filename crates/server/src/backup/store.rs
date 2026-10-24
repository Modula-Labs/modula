//! Where a backup target's objects live, resolved each time it is used:
//!
//! - `s3` targets and R2 bucket services are storage the control plane
//!   reaches ([`Store::Remote`]): it signs, starts, completes and deletes;
//!   nodes only get presigned URLs. An R2 bucket's endpoint and credentials
//!   are its instance's variables (made by `api::buckets`).
//! - A self-hosted bucket service (template `bucket`) listens only on the
//!   private network, which the control plane is not on
//!   ([`Store::Private`]). The node that runs the database does the
//!   requests itself, with the bucket's credentials, sent over mTLS to that
//!   node alone: the data then goes straight from the database's server to
//!   the bucket over the WireGuard mesh (node addresses may reach every
//!   container there), with no node-to-node relay and no extra service.
//!   When both run on the same server, the node uses the bucket container's
//!   address on its environment network, so no mesh is needed then.
//! - A `local` target is one server's disk ([`Store::Disk`]): that server
//!   writes, reads and deletes the files, so only a database it runs can
//!   use it.

use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use secrecy::ExposeSecret;
use zeroize::Zeroizing;

use super::s3;
use crate::deploy::env;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn target_aad(workspace: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: workspace,
        table: "backup_targets",
        column: "secret_access_key",
        row,
    }
}

pub enum Store {
    Remote(s3::Target),
    Private(PrivateBucket),
    Disk { node: Id },
}

/// A self-hosted bucket service and where it runs.
pub struct PrivateBucket {
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret: Zeroizing<String>,
    /// Object key prefix (empty, or ends with `/`).
    pub prefix: String,
    /// Its serving deployment and the server running it.
    pub deployment: Id,
    pub node: Id,
    pub environment: Id,
    pub port: u16,
    /// Its address on the private network, when it is in one.
    pub mesh_ip: Option<String>,
    /// Its service's name (`<name>.modula.internal` in its environment).
    pub service_name: String,
}

impl PrivateBucket {
    /// How the node `from` reaches the bucket: through its container's
    /// address when it runs there, else over the private network.
    pub fn access(&self, from: Id) -> AppResult<pb::S3Access> {
        let endpoint = if from == self.node {
            String::new()
        } else {
            let ip = self.mesh_ip.as_deref().ok_or_else(|| {
                AppError::Conflict(
                    "the bucket runs on another server outside the private network: \
                     run them on the same server, or add both servers to the private network"
                        .into(),
                )
            })?;
            format!("http://{ip}:{}", self.port)
        };
        Ok(pb::S3Access {
            endpoint,
            region: self.region.clone(),
            bucket: self.bucket.clone(),
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret.to_string(),
            deployment_id: self.deployment.to_string(),
            environment_id: self.environment.to_string(),
            port: u32::from(self.port),
        })
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    workspace_id: Id,
    kind: String,
    provider: Option<String>,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    access_key_id: String,
    secret_sealed: Option<Vec<u8>>,
    bucket_instance_id: Option<Id>,
    node_id: Option<Id>,
}

/// A target's store, and the workspace that owns it.
pub async fn load_target(state: &AppState, id: Id) -> AppResult<(Id, Store)> {
    let r: Row = sqlx::query_as(
        "SELECT workspace_id, kind, provider, endpoint, region, bucket, prefix, access_key_id,
                secret_sealed, bucket_instance_id, node_id
         FROM backup_targets WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let store = match (r.kind.as_str(), r.bucket_instance_id, r.node_id) {
        ("bucket", Some(instance), _) => bucket_store(state, instance, &r.prefix).await?,
        ("local", _, Some(node)) => Store::Disk { node },
        _ => {
            let sealed = r
                .secret_sealed
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("target {id} has no secret"))?;
            let row = id.to_string();
            let secret = state
                .keys
                .open(&state.db, sealed, target_aad(r.workspace_id, &row))
                .await?;
            let secret = std::str::from_utf8(&secret).map_err(anyhow::Error::from)?;
            let provider = r.provider.as_deref().unwrap_or_default();
            Store::Remote(s3::Target {
                provider: s3::Provider::parse(provider)
                    .ok_or_else(|| anyhow::anyhow!("unknown provider {provider}"))?,
                endpoint: r.endpoint.parse().map_err(anyhow::Error::from)?,
                region: r.region,
                bucket: r.bucket,
                prefix: r.prefix,
                access_key_id: r.access_key_id,
                secret: Zeroizing::new(secret.to_owned()),
            })
        }
    };
    Ok((r.workspace_id, store))
}

/// The S3 variables of a bucket service instance, references expanded.
pub struct BucketVars {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret: Zeroizing<String>,
}

impl BucketVars {
    pub fn from_env(vars: &env::EnvMap) -> AppResult<Self> {
        let get = |name: &str| -> AppResult<String> {
            vars.get(name)
                .map(|v| v.expose_secret().trim().to_owned())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    AppError::Conflict(format!(
                        "the bucket service has no {name} variable (does it have a bucket in this environment?)"
                    ))
                })
        };
        Ok(Self {
            endpoint: get("S3_ENDPOINT")?,
            region: get("S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
            bucket: get("S3_BUCKET")?,
            access_key_id: get("AWS_ACCESS_KEY_ID")?,
            secret: Zeroizing::new(get("AWS_SECRET_ACCESS_KEY")?),
        })
    }
}

async fn bucket_store(state: &AppState, instance: Id, prefix: &str) -> AppResult<Store> {
    let (template, environment, config, service_name): (
        Option<String>,
        Id,
        serde_json::Value,
        String,
    ) = sqlx::query_as(
        "SELECT s.template, i.environment_id, i.config, s.name FROM service_instances i
             JOIN services s ON s.id = i.service_id WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    let resolved = env::resolve(
        &state.keys,
        &state.db,
        state.cfg.tcp_proxy_host.as_deref(),
        environment,
        instance,
    )
    .await?;
    let vars = BucketVars::from_env(&resolved.vars)?;
    match template.as_deref() {
        Some(crate::api::buckets::R2_TEMPLATE) => {
            // Variables can be edited: the endpoint is checked like any other.
            let region = s3::region(s3::Provider::R2, &vars.region)?;
            Ok(Store::Remote(s3::Target {
                provider: s3::Provider::R2,
                endpoint: s3::endpoint(s3::Provider::R2, &vars.endpoint, &region)?,
                region,
                bucket: s3::bucket(&vars.bucket)?,
                prefix: prefix.to_owned(),
                access_key_id: vars.access_key_id,
                secret: vars.secret,
            }))
        }
        Some("bucket") => {
            let live: Option<(Id, Id, Option<String>)> = sqlx::query_as(
                "SELECT d.id, r.node_id, r.mesh_ip FROM deployments d
                 JOIN deployment_replicas r ON r.deployment_id = d.id AND r.ordinal = 0
                 WHERE d.service_instance_id = $1 AND d.status = 'active'
                 ORDER BY d.created_at DESC LIMIT 1",
            )
            .bind(instance)
            .fetch_optional(&state.db)
            .await?;
            let (deployment, node, mesh_ip) =
                live.ok_or_else(|| AppError::Conflict("the bucket service is not running".into()))?;
            let port = config
                .get("port")
                .and_then(|p| p.as_u64())
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or(9000);
            Ok(Store::Private(PrivateBucket {
                region: s3::region(s3::Provider::Minio, &vars.region)?,
                bucket: s3::bucket(&vars.bucket)?,
                access_key_id: vars.access_key_id,
                secret: vars.secret,
                prefix: prefix.to_owned(),
                deployment,
                node,
                environment,
                port,
                mesh_ip,
                service_name,
            }))
        }
        _ => Err(AppError::Conflict(
            "the backup target's service is not a bucket".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    fn vars(pairs: &[(&str, &str)]) -> env::EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), SecretString::from((*v).to_owned())))
            .collect()
    }

    #[test]
    fn bucket_variables_become_a_target() {
        // What an R2 bucket service's instance holds (api::buckets).
        let r2 = vars(&[
            ("S3_BUCKET", "media"),
            ("BUCKET_NAME", "media"),
            (
                "S3_ENDPOINT",
                "https://0123456789abcdef.r2.cloudflarestorage.com",
            ),
            ("S3_REGION", "auto"),
            ("AWS_ACCESS_KEY_ID", "AKR2"),
            ("AWS_SECRET_ACCESS_KEY", "r2-secret"),
        ]);
        let v = BucketVars::from_env(&r2).unwrap();
        assert_eq!(v.bucket, "media");
        assert_eq!(v.region, "auto");
        assert_eq!(v.access_key_id, "AKR2");
        assert_eq!(v.secret.as_str(), "r2-secret");
        assert!(v.endpoint.ends_with(".r2.cloudflarestorage.com"));

        // An instance without its bucket yet has no credentials.
        let err = BucketVars::from_env(&vars(&[("S3_BUCKET", "media")]))
            .err()
            .unwrap();
        assert!(matches!(err, AppError::Conflict(m) if m.contains("S3_ENDPOINT")));
    }

    #[test]
    fn private_buckets_are_reached_locally_or_over_the_mesh() {
        let (here, there) = (Id::new(), Id::new());
        let mut b = PrivateBucket {
            region: "us-east-1".into(),
            bucket: "bucket".into(),
            access_key_id: "AK".into(),
            secret: Zeroizing::new("sk".into()),
            prefix: "modula-backups/".into(),
            deployment: Id::new(),
            node: here,
            environment: Id::new(),
            port: 9000,
            mesh_ip: Some("10.80.3.7".into()),
            service_name: "files".into(),
        };
        let local = b.access(here).unwrap();
        assert_eq!(local.endpoint, "", "resolved by the node itself");
        assert_eq!(local.deployment_id, b.deployment.to_string());
        assert_eq!(local.port, 9000);
        let remote = b.access(there).unwrap();
        assert_eq!(remote.endpoint, "http://10.80.3.7:9000");
        assert_eq!(remote.secret_access_key, "sk");
        b.mesh_ip = None;
        assert!(matches!(b.access(there), Err(AppError::Conflict(_))));
        assert!(b.access(here).is_ok());
    }
}
