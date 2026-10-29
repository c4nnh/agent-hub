use crate::config::HubConfig;
use sqlx::PgPool;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cfg: HubConfig,
}
