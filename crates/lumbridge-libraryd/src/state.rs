use anyhow::Result;
use lumbridge_library_core::config::Config;
use lumbridge_library_core::models::{Embedder, ModelIdentity, Reranker};
use lumbridge_library_core::{db, status};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub pool: PgPool,
    pub embedder: Embedder,
    pub reranker: Reranker,
    pub token: Arc<String>,
    /// Owner-only actions (sent as `x-library-admin`); None disables them.
    pub admin_token: Option<Arc<String>>,
    /// Currently serving embed model (None until the embed endpoint answers).
    pub embed_model: Arc<RwLock<Option<(i32, ModelIdentity)>>>,
    pub jobs: crate::jobs::Jobs,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Health {
    pub version: String,
    pub db: bool,
    pub embed: bool,
    pub rerank: bool,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EmbedModelInfo {
    pub id: i32,
    pub name: String,
    pub root: String,
    pub dims: usize,
    pub fingerprint: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Status {
    pub version: String,
    pub health: Health,
    pub embed_model: Option<EmbedModelInfo>,
    pub docs: status::ShelfCounts,
    pub warehouse: status::WarehouseCounts,
    pub ledger: status::LedgerCounts,
    pub library_root: String,
}

impl AppState {
    pub async fn new(cfg: Config, pool: PgPool) -> Result<Self> {
        let token = cfg.api_token()?;
        let cfg_admin = cfg.admin_token()?;
        Ok(Self {
            embedder: Embedder::new(cfg.embed.clone()),
            reranker: Reranker::new(cfg.rerank.clone()),
            cfg: Arc::new(cfg),
            pool,
            token: Arc::new(token),
            admin_token: cfg_admin.map(Arc::new),
            embed_model: Arc::new(RwLock::new(None)),
            jobs: crate::jobs::Jobs::default(),
        })
    }

    /// Ask :8012 who it is, register the fingerprint, remember the id.
    pub async fn refresh_embed_model(&self) -> Result<i32> {
        let ident = self.embedder.identity().await?;
        let id = db::register_embed_model(&self.pool, &ident).await?;
        *self.embed_model.write().await = Some((id, ident));
        Ok(id)
    }

    /// True when the request carries the admin token (constant-time compare).
    pub fn is_admin(&self, h: &axum::http::HeaderMap) -> bool {
        let (Some(want), Some(got)) = (&self.admin_token, h.get(lumbridge_library_core::legacy::wire::HEADER_ADMIN).and_then(|v| v.to_str().ok())) else {
            return false;
        };
        crate::constant_time_eq(got.as_bytes(), want.as_bytes())
    }

    /// The embed model id vectors are compared under. Falls back to the most recently seen model
    /// when :8012 is down (search then degrades to full-text on its own).
    pub async fn model_id(&self) -> i32 {
        if let Some((id, _)) = self.embed_model.read().await.as_ref() {
            return *id;
        }
        if let Ok(id) = self.refresh_embed_model().await {
            return id;
        }
        sqlx::query_scalar::<_, i32>("select id from lib.embed_models order by last_seen desc limit 1")
            .fetch_optional(&self.pool)
            .await
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    pub async fn health(&self) -> Health {
        let db_ok = sqlx::query_scalar::<_, i32>("select 1").fetch_one(&self.pool).await.is_ok();
        let embed_ok = self.embedder.identity().await.is_ok();
        let rerank_ok = self.reranker.healthy().await;
        Health { version: lumbridge_library_core::VERSION.into(), db: db_ok, embed: embed_ok, rerank: rerank_ok }
    }

    pub async fn status(&self) -> Result<Status> {
        if self.embed_model.read().await.is_none() {
            let _ = self.refresh_embed_model().await;
        }
        let embed_model = self.embed_model.read().await.as_ref().map(|(id, m)| EmbedModelInfo {
            id: *id,
            name: m.name.clone(),
            root: m.root.clone(),
            dims: m.dims,
            fingerprint: m.fingerprint.clone(),
        });
        Ok(Status {
            version: lumbridge_library_core::VERSION.into(),
            health: self.health().await,
            embed_model,
            docs: status::docs(&self.pool).await?,
            warehouse: status::warehouse(&self.pool).await?,
            ledger: status::ledger(&self.pool).await?,
            library_root: self.cfg.library_root.display().to_string(),
        })
    }
}
