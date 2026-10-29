//! Websocket feed for the inbox, with reconnect and gap-fill.

use super::Session;
use crate::commands::chat::{
    BACKOFF_MAX, check_close, connect_subscribed, fetch_since, next_frame, refusal,
};
use ah_common::{ClientHello, SenderType, WsEvent};
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite;

pub const BACKOFF_START: Duration = Duration::from_secs(1);

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Runs the websocket loop until the hub refuses the feed (4xx, rejected token, lost access),
/// then records the reason on the session. Does nothing for an unbound folder.
pub fn spawn(session: Arc<Session>, backoff_start: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !session.is_bound() {
            return;
        }
        let mut backoff = backoff_start;
        loop {
            let result = connection(&session, &mut backoff, backoff_start).await;
            if let Err(e) = &result
                && let Some(reason) = refusal(e.as_ref())
            {
                tracing::error!(%reason, "hub refused the inbox feed, stopping");
                session.stop_feed(reason);
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
    let hello = ClientHello {
        agent_name: Some(s.agent_name.clone()),
        sender_type: SenderType::Agent,
        folder: Some(s.folder.clone()),
        channels: s.channels.iter().map(|c| c.id).collect(),
    };
    let mut ws = connect_subscribed(&s.client, &hello).await?;
    *backoff = backoff_start;

    for ch in &s.channels {
        let since = s.cursor(ch.id);
        let missed = fetch_since(&s.client, ch.id, since).await?;
        for m in missed {
            s.deliver(m);
        }
    }

    while let Some(frame) = next_frame(&mut ws).await? {
        check_close(&frame)?;
        let tungstenite::Message::Text(t) = frame else {
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
