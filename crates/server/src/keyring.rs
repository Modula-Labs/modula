use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use modula_crypto::envelope::{DataKey, FieldAad, MasterKey};
use sqlx::PgPool;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::error::AppResult;

/// Unwraps and caches per-scope data keys. Keys are created lazily on first use.
pub struct Keyring {
    kek: MasterKey,
    cache: RwLock<HashMap<Uuid, Arc<DataKey>>>,
}

impl Keyring {
    pub fn new(kek: MasterKey) -> Self {
        Self {
            kek,
            cache: RwLock::new(HashMap::new()),
        }
    }

    pub async fn data_key(&self, db: &PgPool, scope: Uuid) -> AppResult<Arc<DataKey>> {
        if let Some(k) = self.cache.read().expect("keyring lock").get(&scope) {
            return Ok(k.clone());
        }
        let fresh = DataKey::generate()?;
        let wrapped = self.kek.wrap(&fresh, scope)?;
        // Racing creators: the first insert wins and everyone reads that row back.
        sqlx::query(
            "INSERT INTO data_keys (scope, kek_id, wrapped) VALUES ($1, $2, $3)
             ON CONFLICT (scope) DO NOTHING",
        )
        .bind(scope)
        .bind(self.kek.id())
        .bind(&wrapped)
        .execute(db)
        .await?;
        let (kek_id, stored): (String, Vec<u8>) =
            sqlx::query_as("SELECT kek_id, wrapped FROM data_keys WHERE scope = $1")
                .bind(scope)
                .fetch_one(db)
                .await?;
        if kek_id != self.kek.id() {
            return Err(anyhow::anyhow!(
                "data key for scope {scope} was wrapped by {kek_id}, but the loaded master key is {}",
                self.kek.id()
            )
            .into());
        }
        let key = Arc::new(self.kek.unwrap(&stored, scope)?);
        self.cache
            .write()
            .expect("keyring lock")
            .insert(scope, key.clone());
        Ok(key)
    }

    pub async fn seal(
        &self,
        db: &PgPool,
        plaintext: &[u8],
        aad: FieldAad<'_>,
    ) -> AppResult<Vec<u8>> {
        Ok(self.data_key(db, aad.scope).await?.seal(plaintext, &aad)?)
    }

    pub async fn open(
        &self,
        db: &PgPool,
        sealed: &[u8],
        aad: FieldAad<'_>,
    ) -> AppResult<Zeroizing<Vec<u8>>> {
        Ok(self.data_key(db, aad.scope).await?.open(sealed, &aad)?)
    }

    /// Drop a scope's cached key (e.g. after its workspace is deleted).
    pub fn forget(&self, scope: Uuid) {
        self.cache.write().expect("keyring lock").remove(&scope);
    }
}
