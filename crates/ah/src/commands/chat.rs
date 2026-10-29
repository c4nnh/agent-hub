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
/// The hub pings every 30s, so a healthy socket never goes this long without a frame. Without
/// it a half-open socket (laptop sleep, NAT drop) would block a read forever.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(75);

/// Next frame, or an error when the socket has been silent longer than `READ_TIMEOUT`.
pub(crate) async fn next_frame<S>(
    ws: &mut S,
) -> Result<Option<tungstenite::Message>, Box<dyn std::error::Error + Send + Sync>>
where
    S: futures_util::Stream<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin,
{
    let frame = tokio::time::timeout(READ_TIMEOUT, ws.next())
        .await
        .map_err(|_| "no frame from hub within the read timeout")?;
    Ok(frame.transpose()?)
}

/// The status of a hub refusal that retrying cannot fix: a 4xx from the REST API, a 401/403 on
/// the websocket handshake, or a policy close (lost access) from the hub.
pub(crate) fn refusal(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    if let Some(crate::client::ClientError::Status { status, .. }) = e.downcast_ref()
        && (400..500).contains(status)
    {
        return Some(refusal_text(*status));
    }
    if let Some(tungstenite::Error::Http(res)) = e.downcast_ref()
        && matches!(res.status().as_u16(), 401 | 403)
    {
        return Some(refusal_text(res.status().as_u16()));
    }
    if let Some(r) = e.downcast_ref::<Refused>() {
        // The hub closes a live socket with this reason after logout or token revocation.
        if r.0 == "token revoked" {
            return Some(refusal_text(401));
        }
        return Some(r.to_string());
    }
    None
}

fn refusal_text(status: u16) -> String {
    match status {
        401 => "hub rejected the login token (401); run `ah auth` to log in again".into(),
        403 => "hub denied access to the channel (403)".into(),
        _ => format!("hub refused the request ({status})"),
    }
}

/// A policy close from the hub: the token was revoked or a channel membership is gone.
#[derive(Debug)]
pub(crate) struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hub closed the connection: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// Maps a close frame to `Refused` when it is a policy close.
pub(crate) fn check_close(frame: &tungstenite::Message) -> Result<(), Refused> {
    if let tungstenite::Message::Close(Some(c)) = frame
        && c.code == tungstenite::protocol::frame::coding::CloseCode::Policy
    {
        return Err(Refused(c.reason.to_string()));
    }
    Ok(())
}

/// Removes control characters (terminal escapes among them) so a message cannot rewrite the
/// user's terminal; newlines and tabs become spaces to keep one message per line.
fn sanitize(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

pub fn render_message(msg: &Message) -> String {
    let prefix = match msg.sender_type {
        SenderType::Agent => ROBOT_PREFIX,
        SenderType::Human => "",
    };
    format!(
        "{prefix}{}: {}",
        sanitize(&msg.sender_name),
        sanitize(&msg.body)
    )
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
    cursor: &mut Cursor,
    backoff: &mut Duration,
    out: &mut impl Write,
) -> Res {
    let last_id = &mut cursor.last_id;
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
            check_close(&frame)?;
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
    let missed = fetch_since(client, channel.id, *last_id).await?;
    let skip = if cursor.is_first {
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
    // Only the first successful connect trims history; a reconnect replays the whole gap, even
    // when the channel was empty before.
    cursor.is_first = false;
    writeln!(out, "-- connected to {} --", channel.name)?;
    out.flush()?;

    while let Some(frame) = next_frame(&mut ws)
        .await
        .map_err(|e| e as Box<dyn std::error::Error>)?
    {
        check_close(&frame)?;
        let tungstenite::Message::Text(t) = frame else {
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

/// Where the chat is in the channel. `is_first` is separate from `last_id == 0`: an empty channel
/// also has id 0, and its reconnect must replay everything, not only the last lines.
struct Cursor {
    last_id: i64,
    is_first: bool,
}

/// Runs until the hub refuses the connection (bad token, lost access); reconnects with backoff
/// otherwise.
pub async fn subscribe_loop(
    client: &HubClient,
    channel: &ChannelSummary,
    out: &mut impl Write,
) -> Res {
    let mut cursor = Cursor {
        last_id: 0,
        is_first: true,
    };
    let mut backoff = BACKOFF_START;
    loop {
        let result = session(client, channel, &mut cursor, &mut backoff, out).await;
        if let Err(e) = &result
            && let Some(reason) = refusal(e.as_ref())
        {
            return Err(reason.into());
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
