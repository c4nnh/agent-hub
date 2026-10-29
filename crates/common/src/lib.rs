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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSummary {
    pub id: uuid::Uuid,
    pub name: String,
    pub role: String,
    pub owner_email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateChannel {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteRequest {
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RespondInvitation {
    pub accept: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitationSummary {
    pub id: uuid::Uuid,
    pub channel_name: String,
    pub invited_by_email: String,
}
