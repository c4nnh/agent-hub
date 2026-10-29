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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderType {
    Human,
    Agent,
}

impl SenderType {
    pub fn as_str(self) -> &'static str {
        match self {
            SenderType::Human => "human",
            SenderType::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub channel_id: uuid::Uuid,
    pub sender_type: SenderType,
    pub sender_name: String,
    pub body: String,
    pub reply_to: Option<i64>,
    pub mentions: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostMessage {
    pub body: String,
    #[serde(default)]
    pub reply_to: Option<i64>,
    #[serde(default)]
    pub mentions: Vec<String>,
    pub sender_type: SenderType,
    pub sender_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentPresence {
    pub name: String,
    pub folder: Option<String>,
    pub connected_at: chrono::DateTime<chrono::Utc>,
}

/// First (and only) frame a WebSocket client sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    pub agent_name: Option<String>,
    pub sender_type: SenderType,
    pub folder: Option<String>,
    pub channels: Vec<uuid::Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    /// Sent once, right after the hub registered the connection's subscriptions.
    Subscribed,
    Message {
        message: Message,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEntry {
    pub id: uuid::Uuid,
    pub channel_id: uuid::Uuid,
    pub title: String,
    pub body: String,
    pub version: i32,
    pub updated_by: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEntrySummary {
    pub id: uuid::Uuid,
    pub title: String,
    pub version: i32,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertContext {
    #[serde(default)]
    pub id: Option<uuid::Uuid>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub base_version: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHit {
    pub entry: ContextEntrySummary,
    pub score: f32,
}
