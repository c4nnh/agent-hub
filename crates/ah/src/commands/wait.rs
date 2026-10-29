//! `ah wait`: block until a message from someone else arrives, print it as JSON lines, exit.
//! A harness that wakes an agent when a background command ends gets push delivery from it.

use crate::{
    bindings::Bindings,
    client::HubClient,
    commands::chat::{
        BACKOFF_MAX, BACKOFF_START, check_close, connect_subscribed, fetch_since, next_frame,
        refusal,
    },
    cursors::{Cursors, PidGuard},
    mcp::{UNBOUND, default_agent_name},
};
use ah_common::{ChannelSummary, ClientHello, Message, SenderType, WsEvent};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_tungstenite::tungstenite;
use uuid::Uuid;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// How long to keep collecting already-queued messages after the first one arrives.
const DRAIN: Duration = Duration::from_millis(200);

/// The feed cannot work and retrying will not help (unbound folder, bad token, lost access).
/// `main` maps it to exit code 2.
#[derive(Debug)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

#[derive(Default)]
pub struct WaitArgs {
    pub folder: Option<PathBuf>,
    pub channel: Option<String>,
    pub name: Option<String>,
    /// Test hook: overrides the drain window.
    #[doc(hidden)]
    pub drain: Option<Duration>,
}

#[doc(hidden)] // public only so tests/wait.rs can exercise the buffer invariant
pub struct Waiter<'a> {
    pub client: &'a HubClient,
    pub agent_name: String,
    pub folder: String,
    pub channels: Vec<ChannelSummary>,
    pub cursors: BTreeMap<Uuid, i64>,
    /// Accepted messages not yet printed. Invariant: a cursor only runs ahead of messages that
    /// are already printed or sit in this buffer, which `next_messages` returns even when a
    /// later step errors. Never hold accepted messages in a local that an error can drop.
    pub pending: Vec<Message>,
    pub drain: Duration,
}

impl Waiter<'_> {
    /// Moves the cursor past `msg` and buffers it unless it is old or sent by this agent.
    /// ponytail: self filter is by name, so two sessions sharing a name do not hear each other.
    /// It also misses our own posts when `ah mcp` runs with a custom --name and this command
    /// does not get the same one; pass the same --name to both.
    fn accept(&mut self, msg: Message) {
        let cursor = self.cursors.entry(msg.channel_id).or_insert(0);
        if msg.id <= *cursor {
            return;
        }
        *cursor = msg.id;
        if msg.sender_name != self.agent_name {
            self.pending.push(msg);
        }
    }

    pub async fn gap_check(&mut self) -> Result<(), Error> {
        for ch in self.channels.clone() {
            let since = self.cursors.get(&ch.id).copied().unwrap_or(0);
            for m in fetch_since(self.client, ch.id, since).await? {
                self.accept(m);
            }
        }
        Ok(())
    }

    /// One attempt: gap-check, then park on a websocket. Returns Ok with `pending` empty when the
    /// socket closed.
    async fn attempt(&mut self, backoff: &mut Duration) -> Result<(), Error> {
        self.gap_check().await?;
        if !self.pending.is_empty() {
            return Ok(());
        }
        let hello = ClientHello {
            agent_name: Some(self.agent_name.clone()),
            sender_type: SenderType::Agent,
            folder: Some(self.folder.clone()),
            channels: self.channels.iter().map(|c| c.id).collect(),
        };
        let mut ws = connect_subscribed(self.client, &hello).await?;
        *backoff = BACKOFF_START;
        // A message posted between the first gap-check and the subscription is only in the REST log.
        self.gap_check().await?;
        if !self.pending.is_empty() {
            return Ok(());
        }
        loop {
            let frame = if self.pending.is_empty() {
                next_frame(&mut ws).await?
            } else {
                match tokio::time::timeout(self.drain, next_frame(&mut ws)).await {
                    Ok(frame) => frame?,
                    Err(_) => return Ok(()),
                }
            };
            let Some(frame) = frame else {
                return Ok(());
            };
            check_close(&frame)?;
            let tungstenite::Message::Text(t) = frame else {
                continue;
            };
            match serde_json::from_str(&t) {
                Ok(WsEvent::Message { message }) => self.accept(message),
                Ok(_) => {}
                Err(error) => eprintln!("warning: unreadable frame: {error}"),
            }
        }
    }

    /// Retries transient failures with backoff; returns at least one message or a `Refusal`.
    async fn next_messages(&mut self) -> Result<Vec<Message>, Refusal> {
        let mut backoff = BACKOFF_START;
        loop {
            let result = self.attempt(&mut backoff).await;
            if !self.pending.is_empty() {
                return Ok(std::mem::take(&mut self.pending));
            }
            match result {
                Ok(()) => eprintln!("connection closed; reconnecting in {backoff:?}"),
                Err(e) => {
                    if let Some(reason) = refusal(e.as_ref()) {
                        return Err(Refusal(reason));
                    }
                    eprintln!("connection lost: {e}; reconnecting in {backoff:?}");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    fn channel_name(&self, id: Uuid) -> &str {
        self.channels
            .iter()
            .find(|c| c.id == id)
            .map_or("unknown", |c| c.name.as_str())
    }
}

/// Waits for the first message from someone else on the folder's channels, prints it and
/// whatever else is already queued as JSON lines, and saves the cursors.
pub async fn run_wait(
    dir: &Path,
    client: &HubClient,
    args: WaitArgs,
    out: &mut impl std::io::Write,
) -> Result<(), Error> {
    let cwd = match args.folder {
        Some(f) => {
            std::fs::canonicalize(&f).map_err(|e| format!("--folder {}: {e}", f.display()))?
        }
        None => std::env::current_dir()?,
    };
    let folder = Bindings::key(&cwd)?;
    // Tells the Claude Code Stop hook this folder is listening; removed on every return.
    let pid_guard = PidGuard::write(dir, &folder);
    if let Err(e) = &pid_guard {
        eprintln!("warning: could not write the listener pidfile: {e}");
    }
    let bindings = Bindings::load(dir)?;
    let binding = bindings.for_folder(&cwd).cloned().unwrap_or_default();
    let bound = if binding.channel_ids.is_empty() {
        Vec::new()
    } else {
        let all = refused(client.list_channels().await)?;
        all.into_iter()
            .filter(|c| binding.channel_ids.contains(&c.id))
            .collect()
    };
    if bound.is_empty() {
        return Err(Refusal(UNBOUND.into()).into());
    }
    let channels: Vec<ChannelSummary> = match &args.channel {
        None => bound,
        Some(name) => {
            let one: Vec<_> = bound.into_iter().filter(|c| &c.name == name).collect();
            if one.is_empty() {
                return Err(Refusal(format!("channel {name} is not bound to this folder")).into());
            }
            one
        }
    };

    let mut cursors = Cursors::load_folder(dir, &folder)?;
    for ch in &channels {
        // First run: history is onboarding's job, so start at the current tail.
        cursors.entry(ch.id).or_insert(ch.last_message_id);
    }
    Cursors::seed(dir, &folder, &cursors)?;

    let mut waiter = Waiter {
        client,
        agent_name: args.name.unwrap_or_else(|| default_agent_name(&cwd)),
        folder,
        channels,
        cursors,
        pending: Vec::new(),
        drain: args.drain.unwrap_or(DRAIN),
    };
    let messages = waiter.next_messages().await?;
    for m in &messages {
        let line = serde_json::json!({
            "channel": waiter.channel_name(m.channel_id),
            "id": m.id,
            "sender_name": m.sender_name,
            "sender_type": m.sender_type,
            "body": m.body,
            "reply_to": m.reply_to,
            "mentions": m.mentions,
            "created_at": m.created_at,
        });
        writeln!(out, "{line}")?;
    }
    out.flush()?;
    Cursors::advance(dir, &waiter.folder, &waiter.cursors)?;
    Ok(())
}

/// Turns a REST 4xx into a `Refusal`; other errors pass through.
fn refused<T>(r: Result<T, crate::client::ClientError>) -> Result<T, Error> {
    r.map_err(|e| match refusal(&e) {
        Some(reason) => Refusal(reason).into(),
        None => e.into(),
    })
}
