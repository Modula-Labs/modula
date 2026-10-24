//! R2 buckets with their own S3 credentials, made through the Cloudflare API.
//!
//! The R2 API can't mint S3 access keys directly. Instead each bucket gets an
//! account-owned API token whose only permissions are "Workers R2 Storage
//! Bucket Item Read/Write" on that one bucket; R2 accepts such a token as an
//! S3 key pair (access key id = the token's id, secret access key = the hex
//! SHA-256 of its value). The token's value is shown once, at creation, and
//! only its derived secret is kept (as a variable).

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::client::{Client, Error, Result};

/// The permission groups a bucket token gets, by their exact names.
pub const BUCKET_PERMISSIONS: [&str; 2] = [
    "Workers R2 Storage Bucket Item Read",
    "Workers R2 Storage Bucket Item Write",
];

/// What the connected token needs to create bucket tokens, as the
/// dashboard names it.
pub const TOKENS_PERMISSION: &str = "Account · Account API Tokens · Edit";
/// What it needs to create, list and delete buckets.
pub const R2_PERMISSION: &str = "Account · Workers R2 Storage · Edit";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bucket {
    pub name: String,
    #[serde(default)]
    pub creation_date: Option<String>,
    /// Location hint, e.g. `WEUR`.
    #[serde(default)]
    pub location: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PermissionGroup {
    pub id: String,
    pub name: String,
}

/// S3 credentials derived from a bucket token.
#[derive(Clone)]
pub struct Credentials {
    pub token_id: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("token_id", &self.token_id)
            .finish_non_exhaustive()
    }
}

/// R2's naming rules: 3–63 lowercase letters, digits and hyphens, starting
/// and ending with a letter or digit.
pub fn valid_bucket_name(name: &str) -> bool {
    let b = name.as_bytes();
    (3..=63).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

/// The policy resource naming one bucket (default jurisdiction).
pub fn bucket_resource(account_id: &str, bucket: &str) -> String {
    format!("com.cloudflare.edge.r2.bucket.{account_id}_default_{bucket}")
}

/// R2's S3 secret access key for a token value.
pub fn secret_access_key(token_value: &str) -> String {
    hex::encode(Sha256::digest(token_value.as_bytes()))
}

impl Client {
    /// The account's buckets (the first 1000, R2's page maximum).
    pub async fn r2_buckets(&self, account_id: &str) -> Result<Vec<Bucket>> {
        #[derive(Deserialize)]
        struct Listing {
            #[serde(default)]
            buckets: Vec<Bucket>,
        }
        let path = format!("accounts/{account_id}/r2/buckets");
        let (listing, _): (Option<Listing>, _) = self
            .call(Method::GET, &path, &[("per_page", "1000".into())], None)
            .await?;
        Ok(listing.map(|l| l.buckets).unwrap_or_default())
    }

    /// Deletes an (empty) bucket; one already gone is fine.
    pub async fn delete_r2_bucket(&self, account_id: &str, name: &str) -> Result<()> {
        let path = format!("accounts/{account_id}/r2/buckets/{name}");
        match self.call::<Value>(Method::DELETE, &path, &[], None).await {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Permission groups account-owned tokens may hold.
    pub async fn token_permission_groups(&self, account_id: &str) -> Result<Vec<PermissionGroup>> {
        self.list(
            &format!("accounts/{account_id}/tokens/permission_groups"),
            &[],
        )
        .await
    }

    /// Creates an account-owned token with read/write access to the objects
    /// of one bucket, and derives its S3 credentials.
    pub async fn create_bucket_token(
        &self,
        account_id: &str,
        bucket: &str,
        name: &str,
    ) -> Result<Credentials> {
        let groups = self.token_permission_groups(account_id).await?;
        let mut ids = Vec::with_capacity(BUCKET_PERMISSIONS.len());
        for wanted in BUCKET_PERMISSIONS {
            let g = groups.iter().find(|g| g.name == wanted).ok_or_else(|| {
                Error::Decode(format!("no {wanted:?} permission group in the account"))
            })?;
            ids.push(json!({ "id": g.id }));
        }
        let body = json!({
            "name": name,
            "policies": [{
                "effect": "allow",
                "resources": { bucket_resource(account_id, bucket): "*" },
                "permission_groups": ids,
            }],
        });
        #[derive(Deserialize)]
        struct Created {
            id: String,
            value: String,
        }
        let path = format!("accounts/{account_id}/tokens");
        let (created, _): (Option<Created>, _) =
            self.call(Method::POST, &path, &[], Some(&body)).await?;
        let created =
            created.ok_or_else(|| Error::Decode("POST tokens: empty result".to_owned()))?;
        Ok(Credentials {
            access_key_id: created.id.clone(),
            secret_access_key: secret_access_key(&created.value),
            token_id: created.id,
        })
    }

    /// Revokes an account-owned token; one already gone is fine.
    pub async fn delete_account_token(&self, account_id: &str, token_id: &str) -> Result<()> {
        let path = format!("accounts/{account_id}/tokens/{token_id}");
        match self.call::<Value>(Method::DELETE, &path, &[], None).await {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_secrets() {
        for ok in ["abc", "my-uploads", "a1-b2", &"a".repeat(63)] {
            assert!(valid_bucket_name(ok), "{ok}");
        }
        for bad in ["ab", "-abc", "abc-", "ABC", "a_b", "a.b", &"a".repeat(64)] {
            assert!(!valid_bucket_name(bad), "{bad}");
        }
        assert_eq!(
            bucket_resource("acc1", "media"),
            "com.cloudflare.edge.r2.bucket.acc1_default_media"
        );
        // sha256("abc")
        assert_eq!(
            secret_access_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
