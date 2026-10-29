//! `ah mcp`: stdio MCP server that lets a coding agent talk to its channels.

pub mod instructions;
pub mod tools;
pub mod wsclient;

use crate::{
    bindings::Bindings,
    client::{ClientError, HubClient},
    commands::chat::fetch_since,
    config::{CliConfig, Credentials},
};
use ah_common::{ChannelSummary, Message};
use rmcp::ServiceExt;
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;
use uuid::Uuid;

/// Shown by every tool when the folder has no channel binding.
pub const UNBOUND: &str = "this folder is not bound to a channel; run: ah channel join";

/// How many ids of this session's own sent messages are remembered for the echo filter.
const SENT_IDS: usize = 1024;

#[derive(Default)]
struct Inbox {
    queue: VecDeque<Message>,
    /// Highest message id seen per channel, delivered or not.
    cursors: HashMap<Uuid, i64>,
    /// Ids returned by this session's send_message calls, newest last.
    sent: VecDeque<i64>,
    /// Set when the hub refused the feed for good; wait_for_message reports it.
    feed_stopped: Option<String>,
}

/// State shared by the tools and the websocket task. Lives as long as the MCP server.
pub struct Session {
    pub client: HubClient,
    pub agent_name: String,
    pub folder: String,
    pub extra_folders: Vec<String>,
    /// Joined channels bound to the folder; empty means unbound.
    pub channels: Vec<ChannelSummary>,
    inbox: Mutex<Inbox>,
    arrived: Notify,
}

impl Session {
    /// Resolves the folder binding and records the latest message id of each joined channel, so
    /// everything posted after this call reaches the inbox.
    pub async fn start(
        client: HubClient,
        bindings: &Bindings,
        cwd: &Path,
        agent_name: String,
    ) -> Result<Arc<Self>, ClientError> {
        let binding = bindings.for_folder(cwd).cloned().unwrap_or_default();
        let mut channels = Vec::new();
        if !binding.channel_ids.is_empty() {
            let all = client.list_channels().await?;
            channels = all
                .into_iter()
                .filter(|c| binding.channel_ids.contains(&c.id))
                .collect();
        }
        let mut cursors = HashMap::new();
        for ch in &channels {
            // ponytail: pages the whole history once per start; a latest-id endpoint would cut it.
            let history = fetch_since(&client, ch.id, 0).await?;
            cursors.insert(ch.id, history.last().map_or(0, |m| m.id));
        }
        let folder = Bindings::key(cwd).unwrap_or_else(|_| cwd.to_string_lossy().into_owned());
        Ok(Arc::new(Self {
            client,
            agent_name,
            folder,
            extra_folders: binding.extra_folders,
            channels,
            inbox: Mutex::new(Inbox {
                cursors,
                ..Inbox::default()
            }),
            arrived: Notify::new(),
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inbox> {
        self.inbox.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_bound(&self) -> bool {
        !self.channels.is_empty()
    }

    pub fn cursor(&self, channel_id: Uuid) -> i64 {
        self.lock().cursors.get(&channel_id).copied().unwrap_or(0)
    }

    pub fn unread(&self) -> usize {
        self.lock().queue.len()
    }

    /// Queues `msg` unless already seen or sent by this session; the cursor moves either way.
    /// Echoes are matched by id, not name: two sessions with the same agent name must still hear
    /// each other.
    pub fn deliver(&self, msg: Message) {
        let mut inbox = self.lock();
        let cursor = inbox.cursors.entry(msg.channel_id).or_insert(0);
        if msg.id <= *cursor {
            return;
        }
        *cursor = msg.id;
        let is_echo = inbox.sent.contains(&msg.id);
        if is_echo {
            return;
        }
        inbox.queue.push_back(msg);
        drop(inbox);
        self.arrived.notify_one();
    }

    /// Records a message this session sent so its push is not delivered back. The push can beat
    /// the HTTP response, so an already-queued copy is removed too.
    pub fn record_sent(&self, id: i64) {
        let mut inbox = self.lock();
        inbox.queue.retain(|m| m.id != id);
        inbox.sent.push_back(id);
        if inbox.sent.len() > SENT_IDS {
            inbox.sent.pop_front();
        }
    }

    /// Marks the feed as dead and wakes a parked wait so it can report `reason`.
    pub fn stop_feed(&self, reason: String) {
        self.lock().feed_stopped = Some(reason);
        self.arrived.notify_one();
    }

    pub fn feed_stopped(&self) -> Option<String> {
        self.lock().feed_stopped.clone()
    }

    /// Everything queued, waiting up to `timeout` for the first message. Empty on timeout, or at
    /// once when the feed has stopped.
    pub async fn wait(&self, timeout: Duration) -> Vec<Message> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (drained, is_stopped) = {
                let mut inbox = self.lock();
                let drained: Vec<Message> = inbox.queue.drain(..).collect();
                (drained, inbox.feed_stopped.is_some())
            };
            if !drained.is_empty() || is_stopped {
                return drained;
            }
            // notify_one keeps a permit when nobody waits, so a push between the drain and here
            // still wakes this call.
            let woke = tokio::time::timeout_at(deadline, self.arrived.notified()).await;
            if woke.is_err() {
                return self.lock().queue.drain(..).collect();
            }
        }
    }

    pub fn channel_name(&self, id: Uuid) -> &str {
        self.channels
            .iter()
            .find(|c| c.id == id)
            .map_or("unknown", |c| c.name.as_str())
    }
}

/// Longest name the hub accepts for a sender or agent.
const MAX_NAME_CHARS: usize = 128;

/// `<harness>@<hostname>/<folder name>`, harness detected from the environment the agent runs in.
/// The folder keeps two sessions on one machine apart in the channel.
pub fn default_agent_name(folder: &Path) -> String {
    let harness = if std::env::var("CLAUDECODE").as_deref() == Ok("1") {
        "claude-code"
    } else if std::env::var_os("CURSOR_AGENT").is_some() {
        "cursor"
    } else {
        "agent"
    };
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    let base = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{harness}@{host}/{base}")
        .chars()
        .take(MAX_NAME_CHARS)
        .collect()
}

/// Serves MCP on stdio until the client disconnects. Logs go to stderr: stdout is the protocol.
pub async fn run(
    dir: &Path,
    cfg: &CliConfig,
    name: Option<String>,
    folder: Option<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    let creds = Credentials::load(dir)?.ok_or("not logged in; run `ah auth`")?;
    let client = HubClient::new(cfg.hub_url.clone(), Some(creds.token));
    let bindings = Bindings::load(dir)?;
    // Canonicalized up front: a path that does not exist (an unexpanded `${workspaceFolder}`)
    // fails here instead of leaving the session silently unbound.
    let cwd = match folder {
        Some(f) => {
            std::fs::canonicalize(&f).map_err(|e| format!("--folder {}: {e}", f.display()))?
        }
        None => std::env::current_dir()?,
    };
    let agent_name = name.unwrap_or_else(|| default_agent_name(&cwd));
    let session = Session::start(client, &bindings, &cwd, agent_name).await?;
    let _ws = wsclient::spawn(session.clone(), wsclient::BACKOFF_START);
    let server = tools::AgentHub::new(session)
        .serve(rmcp::transport::stdio())
        .await?;
    server.waiting().await?;
    Ok(())
}
