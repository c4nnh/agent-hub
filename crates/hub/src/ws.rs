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

    fn agents(&self, channel_id: Uuid) -> Vec<AgentPresence> {
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
    let user = authenticate(&s.pool, &token).await;
    let Some(user) = user else {
        tracing::warn!("ws connect with unknown token");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    ws.on_upgrade(move |socket| run(socket, s, user))
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

async fn run(mut socket: WebSocket, s: AppState, user: User) {
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
    for channel_id in &hello.channels {
        let is_member = require_member(&s.pool, user.id, *channel_id).await;
        if is_member.is_err() {
            tracing::warn!(user_id = %user.id, %channel_id, "ws hello for non-member channel");
            close_policy(&mut socket, "not a channel member").await;
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
    serve_connection(&mut socket, &mut rx, s.registry.ping_every).await;
    s.registry.unregister(conn_id);
}

async fn send(socket: &mut WebSocket, frame: WsMessage) -> bool {
    let sent = tokio::time::timeout(SEND_TIMEOUT, socket.send(frame)).await;
    matches!(sent, Ok(Ok(())))
}

async fn serve_connection(
    socket: &mut WebSocket,
    rx: &mut mpsc::Receiver<Arc<str>>,
    ping_every: Duration,
) {
    let subscribed = match serde_json::to_string(&WsEvent::Subscribed) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!(%error, "ws event serialization failed");
            return;
        }
    };
    if !send(socket, WsMessage::Text(subscribed.into())).await {
        return;
    }
    let mut ticker = tokio::time::interval(ping_every);
    ticker.tick().await;
    let mut last_seen = tokio::time::Instant::now();
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                if !send(socket, WsMessage::Text((*event).into())).await {
                    break;
                }
            }
            _ = ticker.tick() => {
                if last_seen.elapsed() > ping_every * 2 {
                    tracing::warn!("ws peer silent for two ping intervals, closing");
                    break;
                }
                if !send(socket, WsMessage::Ping(Vec::new().into())).await {
                    break;
                }
            }
            incoming = socket.recv() => match incoming {
                // Push-only: anything after hello only proves the peer is alive.
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => last_seen = tokio::time::Instant::now(),
            },
        }
    }
}
