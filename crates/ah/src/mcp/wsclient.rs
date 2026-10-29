//! Websocket feed for the inbox, with reconnect and gap-fill.

use super::Session;
use crate::{
    client::ClientError,
    commands::chat::{BACKOFF_MAX, SUBSCRIBE_TIMEOUT, fetch_since},
};
use ah_common::{ClientHello, SenderType, WsEvent};
use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::{connect_async, tungstenite};

pub const BACKOFF_START: Duration = Duration::from_secs(1);

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Runs the websocket loop until a 4xx from the hub. Does nothing for an unbound folder.
pub fn spawn(session: Arc<Session>, backoff_start: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !session.is_bound() {
            return;
        }
        let mut backoff = backoff_start;
        loop {
            let result = connection(&session, &mut backoff, backoff_start).await;
            if let Err(e) = &result
                && let Some(ClientError::Status { status, .. }) = e.downcast_ref()
                && (400..500).contains(status)
            {
                tracing::error!(status, error = %e, "hub refused the inbox feed, stopping");
                return;
            }
            match result {
                Ok(()) => tracing::warn!(?backoff, "websocket closed, reconnecting"),
                Err(error) => tracing::warn!(%error, ?backoff, "websocket lost, reconnecting"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    })
}

/// One connection: subscribe, gap-fill every channel from its cursor, then queue pushes.
/// Pushes that arrive during the gap-fill wait in the socket and are deduplicated by id.
async fn connection(
    s: &Session,
    backoff: &mut Duration,
    backoff_start: Duration,
) -> Result<(), Error> {
    let (mut ws, _) = connect_async(s.client.ws_url()?).await?;
    let hello = ClientHello {
        agent_name: Some(s.agent_name.clone()),
        sender_type: SenderType::Agent,
        folder: Some(s.folder.clone()),
        channels: s.channels.iter().map(|c| c.id).collect(),
    };
    ws.send(tungstenite::Message::text(serde_json::to_string(&hello)?))
        .await?;
    tokio::time::timeout(SUBSCRIBE_TIMEOUT, async {
        loop {
            let frame = ws
                .next()
                .await
                .ok_or("connection closed before subscribed")??;
            if let tungstenite::Message::Text(t) = frame
                && matches!(serde_json::from_str(&t), Ok(WsEvent::Subscribed))
            {
                return Ok::<(), Error>(());
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for subscription")??;
    *backoff = backoff_start;

    for ch in &s.channels {
        let since = s.cursor(ch.id);
        let missed = fetch_since(&s.client, ch.id, since).await?;
        for m in missed {
            s.deliver(m);
        }
    }

    while let Some(frame) = ws.next().await {
        let tungstenite::Message::Text(t) = frame? else {
            continue;
        };
        match serde_json::from_str(&t) {
            Ok(WsEvent::Message { message }) => s.deliver(message),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "unreadable websocket frame"),
        }
    }
    Ok(())
}
