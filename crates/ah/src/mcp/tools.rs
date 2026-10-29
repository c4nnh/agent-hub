//! MCP tools. Each tool returns text; errors come back as tool errors, never as panics.

use super::{Session, UNBOUND, instructions};
use crate::{client::ClientError, commands::chat::fetch_since};
use ah_common::{ChannelSummary, Message, PostMessage, SenderType, UpsertContext};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{future::Future, sync::Arc, time::Duration};
use uuid::Uuid;

const DEFAULT_WAIT_SECS: u64 = 50;
const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;
const STALE: &str = "stale version: merge with the current entry and retry with its version";

type ToolResult = Result<String, String>;

#[derive(Deserialize, JsonSchema)]
pub struct SendMessageArgs {
    /// Channel name.
    channel: String,
    /// Plain text, no emoji.
    body: String,
    /// Id of the message this answers.
    reply_to: Option<i64>,
    /// Names mentioned with @name in the body.
    mentions: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
pub struct WaitArgs {
    /// Seconds to wait, 1 to 300. Default 50.
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetMessagesArgs {
    channel: String,
    /// Return messages with a larger id, oldest first. Omit to get the most recent messages.
    since_id: Option<i64>,
    /// At most this many messages, up to 500. Default 100.
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ChannelArgs {
    channel: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchContextArgs {
    channel: String,
    query: String,
    /// Number of hits, default 5.
    k: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetContextArgs {
    /// Context entry id.
    id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct UpdateContextArgs {
    /// Entry id to update; omit to create a new entry.
    id: Option<String>,
    channel: String,
    title: String,
    body: String,
    /// Version you edited; required with id.
    base_version: Option<i32>,
}

#[derive(Serialize)]
struct Tagged<'a> {
    channel: &'a str,
    #[serde(flatten)]
    message: &'a Message,
}

fn json(value: &impl Serialize) -> ToolResult {
    serde_json::to_string(value).map_err(|e| e.to_string())
}

/// The hub's `message` field when the body is JSON, else status and raw body.
fn hub_error(e: ClientError) -> String {
    let ClientError::Status { status, message } = &e else {
        return e.to_string();
    };
    let parsed: Option<serde_json::Value> = serde_json::from_str(message).ok();
    match parsed.as_ref().and_then(|v| v["message"].as_str()) {
        Some(m) => m.to_string(),
        None => format!("hub returned {status}: {message}"),
    }
}

fn parse_id(id: &str) -> Result<Uuid, String> {
    Uuid::parse_str(id).map_err(|_| format!("invalid id: {id}"))
}

#[derive(Clone)]
pub struct AgentHub {
    session: Arc<Session>,
    tool_router: ToolRouter<Self>,
}

impl AgentHub {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            tool_router: Self::tool_router(),
        }
    }

    /// Runs a tool body unless the folder is unbound, then appends the unread hint.
    async fn run(&self, body: impl Future<Output = ToolResult>) -> CallToolResult {
        let result = if self.session.is_bound() {
            body.await
        } else {
            Err(UNBOUND.to_string())
        };
        let unread = self.session.unread();
        let hint = |t: String| {
            if unread > 0 {
                format!("{t}\nunread: {unread}")
            } else {
                t
            }
        };
        match result {
            Ok(t) => CallToolResult::success(vec![ContentBlock::text(hint(t))]),
            Err(t) => CallToolResult::error(vec![ContentBlock::text(hint(t))]),
        }
    }

    fn channel(&self, name: &str) -> Result<&ChannelSummary, String> {
        let s = &self.session;
        s.channels.iter().find(|c| c.name == name).ok_or_else(|| {
            let joined: Vec<&str> = s.channels.iter().map(|c| c.name.as_str()).collect();
            format!(
                "unknown channel {name}; joined channels: {}",
                joined.join(", ")
            )
        })
    }
}

#[tool_router]
impl AgentHub {
    #[tool(
        description = "Post a message to a joined channel. After sending, go back to waiting for \
                       replies (wait_for_message loop, or a background `ah wait`)."
    )]
    async fn send_message(&self, args: Parameters<SendMessageArgs>) -> CallToolResult {
        let a = args.0;
        self.run(async {
            let ch = self.channel(&a.channel)?;
            let msg = PostMessage {
                body: a.body,
                reply_to: a.reply_to,
                mentions: a.mentions.unwrap_or_default(),
                sender_type: SenderType::Agent,
                sender_name: self.session.agent_name.clone(),
            };
            let sent = self.session.client.post_message(ch.id, &msg).await;
            let sent = sent.map_err(hub_error)?;
            self.session.record_sent(sent.id);
            json(&sent)
        })
        .await
    }

    #[tool(
        description = "Block until messages arrive on any joined channel, or the timeout passes. \
                       Returns all queued messages tagged with their channel. Call it again after \
                       a timeout. Call this in a loop whenever you have no work: it is how other agents reach \
                       you. If your harness wakes you when a background shell command finishes \
                       (Claude Code does), prefer running `ah wait` as a background command \
                       instead; it costs no turns while waiting."
    )]
    async fn wait_for_message(&self, args: Parameters<WaitArgs>) -> CallToolResult {
        let secs = args
            .0
            .timeout_secs
            .unwrap_or(DEFAULT_WAIT_SECS)
            .clamp(1, 300);
        self.run(async {
            let messages = self.session.wait(Duration::from_secs(secs)).await;
            let stopped = self.session.feed_stopped();
            if messages.is_empty()
                && let Some(reason) = stopped
            {
                return Err(format!(
                    "message feed stopped: {reason}. Restart the agent session after fixing it."
                ));
            }
            if messages.is_empty() {
                return Ok(format!(
                    "no new messages within {secs}s; call wait_for_message again"
                ));
            }
            let tagged: Vec<Tagged> = messages
                .iter()
                .map(|m| Tagged {
                    channel: self.session.channel_name(m.channel_id),
                    message: m,
                })
                .collect();
            json(&tagged)
        })
        .await
    }

    #[tool(
        description = "Read channel history in id order: the most recent messages, or the ones \
                       after since_id. For live delivery use wait_for_message or a background \
                       `ah wait`, not repeated get_messages polling."
    )]
    async fn get_messages(&self, args: Parameters<GetMessagesArgs>) -> CallToolResult {
        let a = args.0;
        self.run(async {
            let ch = self.channel(&a.channel)?;
            let limit = a.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
            let client = &self.session.client;
            if let Some(since) = a.since_id {
                let got = client.messages(ch.id, since, limit).await;
                return json(&got.map_err(hub_error)?);
            }
            // ponytail: pages the whole history to find the tail; a newest-first query would cut it.
            let all = fetch_since(client, ch.id, 0).await.map_err(hub_error)?;
            let skip = all.len().saturating_sub(limit as usize);
            json(&&all[skip..])
        })
        .await
    }

    #[tool(description = "Agents connected to a channel, with their folders.")]
    async fn list_agents(&self, args: Parameters<ChannelArgs>) -> CallToolResult {
        self.run(async {
            let ch = self.channel(&args.0.channel)?;
            let got = self.session.client.list_agents(ch.id).await;
            json(&got.map_err(hub_error)?)
        })
        .await
    }

    #[tool(description = "Semantic search over the channel's shared context.")]
    async fn search_context(&self, args: Parameters<SearchContextArgs>) -> CallToolResult {
        let a = args.0;
        self.run(async {
            let ch = self.channel(&a.channel)?;
            let got = self
                .session
                .client
                .search_context(ch.id, &a.query, a.k)
                .await;
            json(&got.map_err(hub_error)?)
        })
        .await
    }

    #[tool(description = "List the channel's shared context entries, newest first.")]
    async fn list_context(&self, args: Parameters<ChannelArgs>) -> CallToolResult {
        self.run(async {
            let ch = self.channel(&args.0.channel)?;
            let got = self.session.client.list_context(ch.id).await;
            json(&got.map_err(hub_error)?)
        })
        .await
    }

    #[tool(description = "Read one shared context entry by id.")]
    async fn get_context(&self, args: Parameters<GetContextArgs>) -> CallToolResult {
        self.run(async {
            let id = parse_id(&args.0.id)?;
            let got = self.session.client.get_context(id).await;
            json(&got.map_err(hub_error)?)
        })
        .await
    }

    #[tool(
        description = "Create a shared context entry, or update one by id with the version you \
                       edited as base_version."
    )]
    async fn update_context(&self, args: Parameters<UpdateContextArgs>) -> CallToolResult {
        let a = args.0;
        self.run(async {
            let ch = self.channel(&a.channel)?;
            let id = a.id.as_deref().map(parse_id).transpose()?;
            let req = UpsertContext {
                id,
                title: a.title,
                body: a.body,
                base_version: a.base_version,
            };
            match self.session.client.upsert_context(ch.id, &req).await {
                Ok(entry) => json(&entry),
                Err(ClientError::Status {
                    status: 409,
                    message,
                }) => Err(format!("{STALE}\ncurrent entry: {message}")),
                Err(e) => Err(hub_error(e)),
            }
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentHub {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agent-hub", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions::build(&self.session))
    }
}
