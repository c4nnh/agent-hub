use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthPollResponse {
    pub token: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeResponse {
    pub email: String,
    pub name: String,
    pub plan: String,
}
