use crate::{
    auth::{CurrentUser, User, authenticate},
    channels::require_member,
    state::AppState,
};
use ah_common::{AgentPresence, ClientHello, Message, SenderType, WsEvent};
use axum::{
    Json,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message as WsMessage, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::Deserialize;
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use uuid::Uuid;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const QUEUE_LEN: usize = 256;
const PING_EVERY: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_POLICY: u16 = 1008;
const MAX_HELLO_CHANNELS: usize = 64;

struct Subscriber {
    conn_id: u64,
    tx: mpsc::Sender<Arc<str>>,
    presence: Option<AgentPresence>,
}

/// channel_id -> live subscribers. Shared by the POST handler (publish) and the WS tasks.
#[derive(Clone)]
pub struct Registry {
    queue_len: usize,
    ping_every: Duration,
    channels: Arc<Mutex<HashMap<Uuid, Vec<Subscriber>>>>,
    next_conn: Arc<AtomicU64>,
    /// Held by the POST handler from insert through publish; see `messages::post`.
    pub(crate) post_order: Arc<tokio::sync::Mutex<()>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_tuning(QUEUE_LEN, PING_EVERY)
    }
}

impl Registry {
    /// Per-connection queue length and ping interval; tests shrink both.
    pub fn with_tuning(queue_len: usize, ping_every: Duration) -> Self {
        Self {
            queue_len,
            ping_every,
            channels: Default::default(),
            next_conn: Default::default(),
            post_order: Default::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Vec<Subscriber>>> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Subscribes one connection to every channel and returns its id.
    fn register(
        &self,
        channels: &[Uuid],
        tx: &mpsc::Sender<Arc<str>>,
        presence: Option<AgentPresence>,
    ) -> u64 {
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let mut map = self.lock();
        for channel_id in channels {
            map.entry(*channel_id).or_default().push(Subscriber {
                conn_id,
                tx: tx.clone(),
                presence: presence.clone(),
            });
        }
        conn_id
    }

    fn unregister(&self, conn_id: u64) {
        Self::unregister_locked(&mut self.lock(), conn_id);
    }

    fn unregister_locked(map: &mut HashMap<Uuid, Vec<Subscriber>>, conn_id: u64) {
        map.retain(|_, subs| {
            subs.retain(|s| s.conn_id != conn_id);
            !subs.is_empty()
        });
    }

    pub fn publish(&self, channel_id: Uuid, message: &Message) {
        let event = WsEvent::Message {
            message: message.clone(),
        };
        let json: Arc<str> = match serde_json::to_string(&event) {
            Ok(json) => json.into(),
            Err(error) => {
                tracing::error!(%error, "ws event serialization failed");
                return;
            }
        };
        let mut map = self.lock();
        let mut evict = Vec::new();
        if let Some(subs) = map.get_mut(&channel_id) {
            subs.retain(|s| match s.tx.try_send(json.clone()) {
                Err(mpsc::error::TrySendError::Closed(_)) => false,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    evict.push(s.conn_id);
                    true
                }
                Ok(()) => true,
            });
        }
        // A subscriber that cannot keep up is disconnected so it reconnects and
        // catches up via GET since_id; silently skipping events would be undetectable.
        for conn_id in evict {
            tracing::warn!(%channel_id, conn_id, "ws queue full, evicting subscriber");
            Self::unregister_locked(&mut map, conn_id);
        }
    }

    pub(crate) fn agents(&self, channel_id: Uuid) -> Vec<AgentPresence> {
        let map = self.lock();
        let mut found: Vec<AgentPresence> = map
            .get(&channel_id)
            .into_iter()
            .flatten()
            .filter_map(|s| s.presence.clone())
            .collect();
        found.sort_by_key(|a| a.connected_at);
        found
    }
}

pub async fn agents(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<Vec<AgentPresence>>, StatusCode> {
    require_member(&s.pool, u.id, channel_id).await?;
    Ok(Json(s.registry.agents(channel_id)))
}

#[derive(Deserialize)]
pub struct WsQuery {
    token: Option<String>,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
    Query(q): Query<WsQuery>,
) -> Response {
    let Some(token) = q.token else {
        tracing::warn!("ws connect without token");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let user = match authenticate(&s.pool, &token).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            tracing::warn!("ws connect with unknown token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(error) => {
            tracing::error!(%error, "ws token lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    ws.on_upgrade(move |socket| run(socket, s, user, token))
}

/// True when the user belongs to every channel in `channels` (which must be deduplicated).
async fn is_member_of_all(
    pool: &PgPool,
    user_id: Uuid,
    channels: &[Uuid],
) -> Result<bool, sqlx::Error> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from channel_members where user_id = $1 and channel_id = any($2)",
    )
    .bind(user_id)
    .bind(channels)
    .fetch_one(pool)
    .await?;
    Ok(count == channels.len() as i64)
}

/// Re-checks the token and memberships of a live connection. Returns the close reason when
/// access is gone; a database error keeps the connection, since it says nothing about access.
async fn access_lost(
    pool: &PgPool,
    token: &str,
    user_id: Uuid,
    channels: &[Uuid],
) -> Option<&'static str> {
    let user = match authenticate(pool, token).await {
        Ok(user) => user,
        Err(error) => {
            tracing::error!(%error, %user_id, "ws token recheck failed");
            return None;
        }
    };
    if user.is_none_or(|u| u.id != user_id) {
        tracing::warn!(%user_id, "ws token revoked, closing");
        return Some("token revoked");
    }
    match is_member_of_all(pool, user_id, channels).await {
        Ok(true) => None,
        Ok(false) => {
            tracing::warn!(%user_id, "ws channel membership lost, closing");
            Some("not a channel member")
        }
        Err(error) => {
            tracing::error!(%error, %user_id, "ws membership recheck failed");
            None
        }
    }
}

async fn close_policy(socket: &mut WebSocket, reason: &'static str) {
    let frame = CloseFrame {
        code: CLOSE_POLICY,
        reason: reason.into(),
    };
    let _ = socket.send(WsMessage::Close(Some(frame))).await;
}

async fn read_hello(socket: &mut WebSocket) -> Option<ClientHello> {
    loop {
        let frame = tokio::time::timeout(HELLO_TIMEOUT, socket.recv())
            .await
            .ok()?;
        match frame? {
            Ok(WsMessage::Text(text)) => return serde_json::from_str(&text).ok(),
            Ok(WsMessage::Ping(_) | WsMessage::Pong(_)) => continue,
            _ => return None,
        }
    }
}

async fn run(mut socket: WebSocket, s: AppState, user: User, token: String) {
    let Some(mut hello) = read_hello(&mut socket).await else {
        tracing::warn!(user_id = %user.id, "ws missing or invalid hello");
        close_policy(&mut socket, "invalid hello").await;
        return;
    };
    let agent_name = hello
        .agent_name
        .take()
        .filter(|_| hello.sender_type == SenderType::Agent);
    if agent_name
        .as_ref()
        .is_some_and(|n| n.is_empty() || n.chars().count() > 128)
    {
        tracing::warn!(user_id = %user.id, "ws hello has invalid agent name");
        close_policy(&mut socket, "invalid agent name").await;
        return;
    }
    hello.channels.sort();
    hello.channels.dedup();
    if hello.channels.len() > MAX_HELLO_CHANNELS {
        tracing::warn!(user_id = %user.id, count = hello.channels.len(), "ws hello has too many channels");
        close_policy(&mut socket, "too many channels").await;
        return;
    }
    match is_member_of_all(&s.pool, user.id, &hello.channels).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(user_id = %user.id, "ws hello for non-member channel");
            close_policy(&mut socket, "not a channel member").await;
            return;
        }
        Err(error) => {
            // Not a policy close: the client should retry, not give up.
            tracing::error!(%error, user_id = %user.id, "ws membership check failed");
            return;
        }
    }

    let presence = agent_name.map(|name| AgentPresence {
        name,
        folder: hello.folder.clone(),
        connected_at: Utc::now(),
    });
    let (tx, mut rx) = mpsc::channel::<Arc<str>>(s.registry.queue_len);
    let conn_id = s.registry.register(&hello.channels, &tx, presence);
    drop(tx);
    let live = Live {
        pool: &s.pool,
        token: &token,
        user_id: user.id,
        channels: &hello.channels,
    };
    let closed_for = serve_connection(&mut socket, &mut rx, s.registry.ping_every, live).await;
    s.registry.unregister(conn_id);
    if let Some(reason) = closed_for {
        close_policy(&mut socket, reason).await;
    }
}

/// What a live connection needs to re-check its access on each ping tick.
struct Live<'a> {
    pool: &'a PgPool,
    token: &'a str,
    user_id: Uuid,
    channels: &'a [Uuid],
}

async fn send(socket: &mut WebSocket, frame: WsMessage) -> bool {
    let sent = tokio::time::timeout(SEND_TIMEOUT, socket.send(frame)).await;
    matches!(sent, Ok(Ok(())))
}

/// Pushes events until the connection ends. Returns a policy close reason when access was lost.
async fn serve_connection(
    socket: &mut WebSocket,
    rx: &mut mpsc::Receiver<Arc<str>>,
    ping_every: Duration,
    live: Live<'_>,
) -> Option<&'static str> {
    let subscribed = match serde_json::to_string(&WsEvent::Subscribed) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!(%error, "ws event serialization failed");
            return None;
        }
    };
    if !send(socket, WsMessage::Text(subscribed.into())).await {
        return None;
    }
    let mut ticker = tokio::time::interval(ping_every);
    ticker.tick().await;
    let mut last_seen = tokio::time::Instant::now();
    loop {
        tokio::select! {
            event = rx.recv() => {
                // None: the registry dropped this subscriber (queue full), so the client reconnects.
                let event = event?;
                if !send(socket, WsMessage::Text((*event).into())).await {
                    return None;
                }
            }
            _ = ticker.tick() => {
                if last_seen.elapsed() > ping_every * 2 {
                    tracing::warn!("ws peer silent for two ping intervals, closing");
                    return None;
                }
                let lost = access_lost(live.pool, live.token, live.user_id, live.channels).await;
                if lost.is_some() {
                    return lost;
                }
                if !send(socket, WsMessage::Ping(Vec::new().into())).await {
                    return None;
                }
            }
            incoming = socket.recv() => match incoming {
                // Push-only: anything after hello only proves the peer is alive.
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => return None,
                Some(Ok(_)) => last_seen = tokio::time::Instant::now(),
            },
        }
    }
}
