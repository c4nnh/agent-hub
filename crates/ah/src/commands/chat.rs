use crate::{client::HubClient, commands::channel::resolve};
use ah_common::{ChannelSummary, ClientHello, Message, SenderType, WsEvent};
use futures_util::{SinkExt, StreamExt};
use std::{io::Write, time::Duration};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio_tungstenite::{connect_async, tungstenite};

type Res = Result<(), Box<dyn std::error::Error>>;

// the codebase's single allowed emoji: robot prefix for agent senders
pub const ROBOT_PREFIX: &str = "\u{1F916} ";

const HISTORY_LINES: usize = 20;
const PAGE: i64 = 500;
pub(crate) const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_START: Duration = Duration::from_secs(1);
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub fn render_message(msg: &Message) -> String {
    let prefix = match msg.sender_type {
        SenderType::Agent => ROBOT_PREFIX,
        SenderType::Human => "",
    };
    format!("{prefix}{}: {}", msg.sender_name, msg.body)
}

/// All messages after `since_id`, following the hub's page cap.
pub(crate) async fn fetch_since(
    client: &HubClient,
    channel_id: uuid::Uuid,
    mut since_id: i64,
) -> Result<Vec<Message>, crate::client::ClientError> {
    let mut all = Vec::new();
    loop {
        let page = client.messages(channel_id, since_id, PAGE).await?;
        let full = page.len() as i64 == PAGE;
        if let Some(last) = page.last() {
            since_id = last.id;
        }
        all.extend(page);
        if !full {
            return Ok(all);
        }
    }
}

fn print(out: &mut impl Write, msg: &Message, last_id: &mut i64) -> std::io::Result<()> {
    *last_id = msg.id;
    writeln!(out, "{}", render_message(msg))?;
    out.flush()
}

/// One connection: subscribe, fill the gap since `last_id` (or show recent history on the first
/// run), then print pushed messages until the socket closes. Messages pushed during catch-up are
/// deduplicated by id, so nothing is lost or repeated across reconnects.
async fn session(
    client: &HubClient,
    channel: &ChannelSummary,
    last_id: &mut i64,
    backoff: &mut Duration,
    out: &mut impl Write,
) -> Res {
    let (mut ws, _) = connect_async(client.ws_url()?).await?;
    let hello = ClientHello {
        agent_name: None,
        sender_type: SenderType::Human,
        folder: None,
        channels: vec![channel.id],
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
                return Ok::<(), Box<dyn std::error::Error>>(());
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for subscription")??;

    // Subscribed means the connection is healthy, whether or not messages follow.
    *backoff = BACKOFF_START;
    let is_first = *last_id == 0;
    let missed = fetch_since(client, channel.id, *last_id).await?;
    let skip = if is_first {
        missed.len().saturating_sub(HISTORY_LINES)
    } else {
        0
    };
    for (i, m) in missed.iter().enumerate() {
        if i < skip {
            // Older history is not shown, but the cursor still moves past it.
            *last_id = m.id;
        } else {
            print(out, m, last_id)?;
        }
    }
    writeln!(out, "-- connected to {} --", channel.name)?;
    out.flush()?;

    while let Some(frame) = ws.next().await {
        let tungstenite::Message::Text(t) = frame? else {
            continue;
        };
        match serde_json::from_str(&t) {
            Ok(WsEvent::Message { message }) if message.id > *last_id => {
                print(out, &message, last_id)?;
            }
            Ok(_) => {}
            Err(error) => eprintln!("warning: unreadable frame: {error}"),
        }
    }
    Ok(())
}

/// Runs until a non-retryable error (4xx from the hub); reconnects with backoff otherwise.
pub async fn subscribe_loop(
    client: &HubClient,
    channel: &ChannelSummary,
    out: &mut impl Write,
) -> Res {
    let mut last_id = 0;
    let mut backoff = BACKOFF_START;
    loop {
        let result = session(client, channel, &mut last_id, &mut backoff, out).await;
        if let Err(e) = &result
            && let Some(crate::client::ClientError::Status { status, .. }) = e.downcast_ref()
            && (400..500).contains(status)
        {
            return result;
        }
        match result {
            Ok(()) => eprintln!("connection closed; reconnecting in {backoff:?}"),
            Err(e) => eprintln!("connection lost: {e}; reconnecting in {backoff:?}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Name shown on messages sent by this user: profile name, else the email's local part.
fn sender_name(name: &str, email: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        email.split('@').next().unwrap_or(email).to_string()
    } else {
        name.to_string()
    }
}

/// Live chat: prints incoming messages and posts each input line. Returns on input EOF.
pub async fn run_chat(
    client: &HubClient,
    channel: &str,
    input: impl AsyncBufRead + Unpin,
    out: &mut impl Write,
) -> Res {
    let ch = resolve(client, channel).await?;
    let me = client.me().await?;
    let name = sender_name(&me.name, &me.email);
    let mut lines = input.lines();
    let send = async {
        while let Some(line) = lines.next_line().await? {
            let body = line.trim();
            if body.is_empty() {
                continue;
            }
            if let Err(e) = client.post_human_message(ch.id, &name, body).await {
                eprintln!("error: message not sent: {e}");
            }
        }
        Ok::<(), std::io::Error>(())
    };
    tokio::select! {
        r = subscribe_loop(client, &ch, out) => r,
        r = send => Ok(r?),
    }
}
