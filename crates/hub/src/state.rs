use crate::{config::HubConfig, embed::Embedder, ws::Registry};
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cfg: HubConfig,
    pub registry: Registry,
    pub embedder: Arc<dyn Embedder>,
}

impl AppState {
    pub fn new(pool: PgPool, cfg: HubConfig, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            pool,
            cfg,
            registry: Registry::default(),
            embedder,
        }
    }
}
