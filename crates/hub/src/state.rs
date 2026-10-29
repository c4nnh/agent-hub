use crate::{config::HubConfig, ws::Registry};
use sqlx::PgPool;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cfg: HubConfig,
    pub registry: Registry,
}

impl AppState {
    pub fn new(pool: PgPool, cfg: HubConfig) -> Self {
        Self {
            pool,
            cfg,
            registry: Registry::default(),
        }
    }
}
