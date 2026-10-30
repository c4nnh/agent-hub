//! `ah daemon`: wakes idle agents by spawning their harness CLI when channel messages arrive.
//!
//! The daemon never delivers anything. It only reads the shared cursor in `wait-cursors.json`
//! to learn what is undelivered, and it must never call `Cursors::advance` or `Cursors::seed`:
//! delivery belongs to the woken agent's normal path (`ah wait` or the MCP gap-fill), which
//! moves the cursor once the agent has the messages.

use crate::{
    bindings::Bindings,
    client::HubClient,
    commands::{
        chat::{
            BACKOFF_MAX, BACKOFF_START, READ_TIMEOUT, check_close, connect_subscribed, fetch_since,
            next_frame, refusal,
        },
        wait::Refusal,
    },
    cursors::{Cursors, listener_alive},
    mcp::agent_name,
};
use ah_common::{ChannelSummary, ClientHello, SenderType, WsEvent};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Minimum time between two spawns for one folder.
pub const COOLDOWN: Duration = Duration::from_secs(30);
/// How often a parked watcher re-checks the log and reaps its child, so a cooldown that
/// blocked a wake is re-evaluated without waiting for the next message.
const TICK: Duration = Duration::from_secs(15);
/// How often bindings.json is re-read for new, changed or removed wake folders.
const REFRESH: Duration = Duration::from_secs(60);

pub const WAKE_PROMPT: &str = "New Agent Hub messages are waiting in this folder's channels. Read them with the agent-hub MCP tools (wait_for_message returns the backlog immediately), handle them, and reply with send_message. When you are done, finish and exit; the daemon will wake you again when new messages arrive.";

/// Known wake command template for a harness name.
pub fn harness_template(harness: &str) -> Option<&'static str> {
    match harness {
        "cursor" => Some("cursor-agent -p {prompt}"),
        "codex" => Some("codex exec {prompt}"),
        "claude-code" => Some("claude -p {prompt}"),
        _ => None,
    }
}

/// Splits `template` into program and arguments, replacing `{prompt}` with `prompt`. A template
/// without the placeholder gets the prompt appended as one extra argument.
/// ponytail: split on whitespace only, no shell quoting; shell-words parsing is the upgrade path.
pub fn wake_argv(template: &str, prompt: &str) -> Vec<String> {
    let mut found = false;
    let mut argv: Vec<String> = template
        .split_whitespace()
        .map(|token| {
            found |= token.contains("{prompt}");
            token.replace("{prompt}", prompt)
        })
        .collect();
    if !found {
        argv.push(prompt.to_string());
    }
    argv
}

/// Whether the daemon should spawn the wake command now. `max_foreign_id` is the highest
/// undelivered message id from someone else (0 when none).
pub fn should_wake(
    listener_alive: bool,
    child_running: bool,
    max_foreign_id: i64,
    last_woken_id: i64,
    since_last_spawn: Duration,
) -> bool {
    !listener_alive
        && !child_running
        && max_foreign_id > last_woken_id
        && since_last_spawn >= COOLDOWN
}

/// Names this folder's own agents post under; their messages never wake anything.
fn own_names(folder: &str) -> Vec<String> {
    ["agent", "claude-code", "cursor", "codex"]
        .iter()
        .map(|h| agent_name(h, Path::new(folder)))
        .collect()
}

/// Watches one folder's channels and spawns its wake command when nobody is listening.
#[doc(hidden)] // public so tests/daemon.rs can drive single passes
pub struct Watch {
    pub client: Arc<HubClient>,
    pub dir: PathBuf,
    pub folder: String,
    pub command: String,
    pub channels: Vec<ChannelSummary>,
    own_names: Vec<String>,
    /// Highest foreign message id the last spawn was for.
    pub last_woken_id: i64,
    pub last_spawn: Option<Instant>,
    child: Option<Child>,
}

impl Watch {
    pub fn new(
        client: Arc<HubClient>,
        dir: PathBuf,
        folder: String,
        command: String,
        channels: Vec<ChannelSummary>,
    ) -> Self {
        Self {
            own_names: own_names(&folder),
            client,
            dir,
            folder,
            command,
            channels,
            last_woken_id: 0,
            last_spawn: None,
            child: None,
        }
    }

    #[doc(hidden)]
    pub fn has_child(&self) -> bool {
        self.child.is_some()
    }

    fn reap(&mut self) {
        let Some(child) = &mut self.child else {
            return;
        };
        // An error from try_wait means the handle is unusable; treat it as exited.
        if !matches!(child.try_wait(), Ok(None)) {
            self.child = None;
        }
    }

    /// One evaluation: reap the child, read what is undelivered past the shared cursor, and
    /// spawn the wake command when `should_wake` allows it.
    pub async fn pass(&mut self) -> Result<(), Error> {
        self.reap();
        // Order matters: `ah wait` advances the cursor before it drops its pidfile. Checking
        // liveness first and reading the cursor after means a dead listener's delivery is
        // already visible in the cursor.
        let alive = listener_alive(&self.dir, &self.folder);
        let running = self.child.is_some();
        if alive || running {
            return Ok(());
        }
        let cursors = Cursors::load_folder(&self.dir, &self.folder)?;
        let mut newest: Option<(i64, String)> = None;
        for ch in &self.channels {
            // No cursor means the folder was never seeded; start at the tail seen at startup.
            let since = cursors.get(&ch.id).copied().unwrap_or(ch.last_message_id);
            for m in fetch_since(&self.client, ch.id, since).await? {
                let is_own = self.own_names.contains(&m.sender_name);
                let is_newer = newest.as_ref().is_none_or(|(id, _)| m.id > *id);
                if !is_own && is_newer {
                    newest = Some((m.id, ch.name.clone()));
                }
            }
        }
        let (max_foreign_id, channel) = newest.unwrap_or((0, String::new()));
        let since_spawn = self.last_spawn.map_or(Duration::MAX, |t| t.elapsed());
        let allowed = should_wake(
            alive,
            running,
            max_foreign_id,
            self.last_woken_id,
            since_spawn,
        );
        if !allowed {
            return Ok(());
        }
        let has_session = self.own_agent_online().await;
        if has_session {
            return Ok(());
        }
        self.spawn(max_foreign_id, &channel);
        Ok(())
    }

    /// True when one of this folder's own agent names is connected to a bound channel, i.e. an
    /// interactive session is working and must not get a headless twin. A failed lookup counts
    /// as offline: waking is preferred over staying silent.
    async fn own_agent_online(&self) -> bool {
        for ch in &self.channels {
            let listed = self.client.list_agents(ch.id).await;
            match listed {
                Ok(agents) => {
                    if agents.iter().any(|a| self.own_names.contains(&a.name)) {
                        return true;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, folder = %self.folder, "could not check online agents, waking anyway");
                    return false;
                }
            }
        }
        false
    }

    fn spawn(&mut self, message_id: i64, channel: &str) {
        // Recorded before the spawn so a command that fails to start is not retried in a loop.
        self.last_woken_id = message_id;
        self.last_spawn = Some(Instant::now());
        let argv = wake_argv(&self.command, WAKE_PROMPT);
        let Some((program, args)) = argv.split_first() else {
            return;
        };
        // stdout and stderr are inherited: the daemon is foreground and its terminal is the log.
        let spawned = Command::new(program)
            .args(args)
            .current_dir(&self.folder)
            .stdin(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => {
                self.child = Some(child);
                println!(
                    "woke {} ({}) for message {message_id} in #{channel}",
                    self.folder, self.command
                );
            }
            Err(error) => tracing::error!(
                %error,
                folder = %self.folder,
                command = %self.command,
                "wake command failed to start"
            ),
        }
    }

    /// One connection: evaluate, park on the websocket, evaluate on every foreign message and
    /// on every tick. Returns when the socket closes.
    async fn attempt(&mut self, backoff: &mut Duration) -> Result<(), Error> {
        self.pass().await?;
        let hello = ClientHello {
            agent_name: None,
            sender_type: SenderType::Human,
            folder: None,
            channels: self.channels.iter().map(|c| c.id).collect(),
        };
        let mut ws = connect_subscribed(&self.client, &hello).await?;
        *backoff = BACKOFF_START;
        // A message posted between the first pass and the subscription is only in the REST log.
        self.pass().await?;
        let mut last_frame = Instant::now();
        loop {
            let waited = tokio::time::timeout(TICK, next_frame(&mut ws)).await;
            let Ok(frame) = waited else {
                if last_frame.elapsed() > READ_TIMEOUT {
                    return Err("no frame from hub within the read timeout".into());
                }
                self.pass().await?;
                continue;
            };
            let Some(frame) = frame? else {
                return Ok(());
            };
            last_frame = Instant::now();
            check_close(&frame)?;
            let tungstenite::Message::Text(t) = frame else {
                continue;
            };
            match serde_json::from_str(&t) {
                Ok(WsEvent::Message { message }) => {
                    if !self.own_names.contains(&message.sender_name) {
                        self.pass().await?;
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "unreadable websocket frame"),
            }
        }
    }

    /// Reconnects with backoff forever; returns only on a refusal that retrying cannot fix.
    pub async fn run(&mut self) -> Result<(), Refusal> {
        let mut backoff = BACKOFF_START;
        loop {
            let result = self.attempt(&mut backoff).await;
            match result {
                Ok(()) => tracing::warn!(?backoff, "websocket closed, reconnecting"),
                Err(error) => {
                    if let Some(reason) = refusal(error.as_ref()) {
                        return Err(Refusal(reason));
                    }
                    tracing::warn!(%error, ?backoff, "watch lost, reconnecting");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// A watcher is restarted when either part changes.
type WakeConfig = (String, Vec<uuid::Uuid>);

/// Folders with a wake command and at least one bound channel, mapped to command and channels.
fn wake_folders(dir: &Path) -> std::io::Result<HashMap<String, WakeConfig>> {
    let bindings = Bindings::load(dir)?;
    Ok(bindings
        .0
        .into_iter()
        .filter(|(_, b)| !b.channel_ids.is_empty())
        .filter_map(|(folder, b)| Some((folder, (b.wake_command?, b.channel_ids))))
        .collect())
}

async fn watch_folder(
    client: Arc<HubClient>,
    dir: PathBuf,
    folder: String,
    command: String,
) -> Result<(), Error> {
    let bindings = Bindings::load(&dir)?;
    let ids = bindings
        .0
        .get(&folder)
        .map(|b| b.channel_ids.clone())
        .unwrap_or_default();
    let all = client.list_channels().await?;
    let channels: Vec<_> = all.into_iter().filter(|c| ids.contains(&c.id)).collect();
    if channels.is_empty() {
        return Err("none of the bound channels are visible to you".into());
    }
    let mut watch = Watch::new(client, dir, folder, command, channels);
    watch.run().await?;
    Ok(())
}

/// Runs until ctrl-c. Re-reads bindings.json every `REFRESH` and starts, restarts or stops
/// watchers to match.
/// ponytail: a changed wake command or channel list restarts the watcher, so the old child handle is dropped and
/// a still-running agent is not tracked; a live `ah wait` pidfile still prevents a double wake.
pub async fn run_daemon(dir: &Path, client: HubClient) -> Result<(), Error> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    let first = wake_folders(dir)?;
    if first.is_empty() {
        println!(
            "no folder has a wake command; run `ah channel wake --harness <cursor|codex|claude-code>` in a bound folder, then start `ah daemon`"
        );
        return Ok(());
    }
    let client = Arc::new(client);
    let mut tasks: HashMap<String, (WakeConfig, tokio::task::JoinHandle<()>)> = HashMap::new();
    let mut wanted = first;
    loop {
        tasks.retain(|folder, (config, handle)| {
            let keep = wanted.get(folder) == Some(config) && !handle.is_finished();
            if !keep {
                handle.abort();
            }
            keep
        });
        for (folder, config) in &wanted {
            let command = &config.0;
            if tasks.contains_key(folder) {
                continue;
            }
            tracing::info!(%folder, %command, "watching folder");
            let (client, dir) = (client.clone(), dir.to_path_buf());
            let (f, c) = (folder.clone(), command.clone());
            let handle = tokio::spawn(async move {
                let stopped = watch_folder(client, dir, f.clone(), c).await;
                if let Err(error) = stopped {
                    tracing::error!(%error, folder = %f, "watcher stopped; retrying at the next refresh");
                }
            });
            tasks.insert(folder.clone(), (config.clone(), handle));
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(REFRESH) => {}
        }
        match wake_folders(dir) {
            Ok(w) => wanted = w,
            Err(error) => {
                tracing::warn!(%error, "could not re-read bindings; keeping the current watchers")
            }
        }
    }
    for (_, handle) in tasks.into_values() {
        handle.abort();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(t: &str) -> Vec<String> {
        wake_argv(t, "hello world")
    }

    #[test]
    fn prompt_replaces_standalone_token() {
        assert_eq!(
            argv("codex exec {prompt}"),
            ["codex", "exec", "hello world"]
        );
    }

    #[test]
    fn prompt_replaces_inside_a_token() {
        assert_eq!(argv("tool --msg={prompt}"), ["tool", "--msg=hello world"]);
    }

    #[test]
    fn prompt_is_appended_when_missing() {
        assert_eq!(
            argv("my-agent --fast"),
            ["my-agent", "--fast", "hello world"]
        );
    }

    #[test]
    fn extra_whitespace_is_ignored() {
        assert_eq!(argv("  a   {prompt} "), ["a", "hello world"]);
    }

    #[test]
    fn harness_templates_are_known() {
        assert_eq!(harness_template("cursor"), Some("cursor-agent -p {prompt}"));
        assert_eq!(harness_template("codex"), Some("codex exec {prompt}"));
        assert_eq!(harness_template("claude-code"), Some("claude -p {prompt}"));
        assert_eq!(harness_template("vim"), None);
    }

    const LONG: Duration = Duration::from_secs(31);

    #[test]
    fn wakes_when_every_gate_is_open() {
        assert!(should_wake(false, false, 5, 4, LONG));
    }

    #[test]
    fn live_listener_blocks() {
        assert!(!should_wake(true, false, 5, 4, LONG));
    }

    #[test]
    fn running_child_blocks() {
        assert!(!should_wake(false, true, 5, 4, LONG));
    }

    #[test]
    fn already_woken_for_this_id_blocks() {
        assert!(!should_wake(false, false, 5, 5, LONG));
        assert!(!should_wake(false, false, 4, 5, LONG));
        assert!(!should_wake(false, false, 0, 0, LONG));
    }

    #[test]
    fn cooldown_blocks_then_opens() {
        assert!(!should_wake(false, false, 5, 4, Duration::from_secs(29)));
        assert!(should_wake(false, false, 5, 4, COOLDOWN));
    }
}
