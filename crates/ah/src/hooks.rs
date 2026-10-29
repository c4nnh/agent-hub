//! Claude Code and Cursor hooks that keep a channel-bound folder listening.
//!
//! Cursor contract (cursor.com/docs/agent/hooks, "Configuration file", "Hook events > stop"):
//! user hooks live in `~/.cursor/hooks.json` and run from `~/.cursor`; the stop hook gets
//! `workspace_roots` (no `cwd`) on stdin, and on exit 0 a stdout `{"followup_message": "..."}`
//! is submitted as the next user message.
//!
//! Contract (code.claude.com/docs/en/hooks, "Stop input", "Stop decision control",
//! "SessionStart"): the hook gets a JSON object on stdin with `cwd` and, for Stop,
//! `stop_hook_active`. On exit 0, one JSON object on stdout is parsed: Stop blocks with
//! `{"decision":"block","reason":...}`; SessionStart adds
//! `hookSpecificOutput.additionalContext`. No output means no effect.

use crate::{
    bindings::Bindings,
    client::HubClient,
    commands::chat::{announce_line, fetch_since},
    config::{CliConfig, Credentials},
    cursors::{Cursors, listener_alive},
    mcp::{agent_name, default_agent_name},
};
use ah_common::Message;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

pub const STOP_CMD: &str = "ah hook stop";
pub const SESSION_START_CMD: &str = "ah hook session-start";
const EVENTS: [(&str, &str); 2] = [("Stop", STOP_CMD), ("SessionStart", SESSION_START_CMD)];
const NET_TIMEOUT: Duration = Duration::from_secs(2);
const ANNOUNCE: &str = "new message from <sender> in #<channel>: <body>";

#[derive(Debug, PartialEq)]
pub enum HookStatus {
    /// Claude Code not found, or AH_NO_HOOKS=1.
    NotApplicable,
    /// At least one entry was added.
    Installed,
    Unchanged,
}

/// `AH_NO_HOOKS=1` turns automatic installation off.
pub fn opt_out(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// The Claude Code user directory to install into, or None when hooks do not apply.
pub fn detect(claude_dir: PathBuf, claude_on_path: bool, opt_out: bool) -> Option<PathBuf> {
    let present = claude_on_path || claude_dir.is_dir();
    (present && !opt_out).then_some(claude_dir)
}

/// `~/.claude` when Claude Code is installed (the directory exists or `claude` is on PATH)
/// and AH_NO_HOOKS is not 1.
pub fn claude_dir() -> Option<PathBuf> {
    let off = opt_out(std::env::var_os("AH_NO_HOOKS").as_deref());
    detect(user_claude_dir()?, claude_on_path(), off)
}

/// `~/.claude` regardless of detection; uninstall uses it so AH_NO_HOOKS=1 cannot hide
/// entries installed earlier.
pub fn user_claude_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude"))
}

fn claude_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| {
        ["claude", "claude.exe", "claude.cmd"]
            .iter()
            .any(|n| d.join(n).is_file())
    })
}

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("hook settings: {msg}"),
    )
}

fn load(path: &Path) -> std::io::Result<Value> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| invalid(&e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e),
    }
}

/// Atomic: temp file in the target's directory, then rename. A symlinked settings.json is
/// written through to its target, and the target's permissions are kept.
fn save(path: &Path, v: &Value) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = target
        .parent()
        .ok_or_else(|| invalid("no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    let mut text = serde_json::to_string_pretty(v).map_err(std::io::Error::other)?;
    text.push('\n');
    tmp.write_all(text.as_bytes())?;
    if let Ok(meta) = std::fs::metadata(&target) {
        tmp.as_file().set_permissions(meta.permissions())?;
    }
    tmp.persist(&target).map_err(|e| e.error)?;
    Ok(())
}

/// Ours: a command whose program is `ah` (bare, or a path ending in `ah` / `ah.exe`) and
/// whose first argument is `hook`, wherever `ah` lives.
fn is_ours(hook: &Value) -> bool {
    let Some(command) = hook.get("command").and_then(Value::as_str) else {
        return false;
    };
    let mut words = command.split_whitespace();
    let program = words
        .next()
        .unwrap_or("")
        .trim_matches(|c| c == '"' || c == '\'');
    let name = program.rsplit(['/', '\\']).next().unwrap_or("");
    (name == "ah" || name == "ah.exe") && words.next() == Some("hook")
}

fn group_hooks(group: &Value) -> &[Value] {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Adds the two matcher-less entries to `{claude_dir}/settings.json` where no `ah hook ...`
/// command (see `is_ours`) exists yet. Never removes or reorders anything; leaves the file untouched when
/// nothing is missing or when it does not parse.
pub fn ensure_hooks(claude_dir: Option<&Path>) -> std::io::Result<HookStatus> {
    let Some(claude_dir) = claude_dir else {
        return Ok(HookStatus::NotApplicable);
    };
    let path = claude_dir.join("settings.json");
    let mut settings = load(&path)?;
    let root = settings
        .as_object_mut()
        .ok_or_else(|| invalid("not an object"))?;
    let mut changed = false;
    for (event, command) in EVENTS {
        let has_ours = root
            .get("hooks")
            .and_then(|h| h.get(event))
            .and_then(Value::as_array)
            .is_some_and(|groups| groups.iter().flat_map(group_hooks).any(is_ours));
        if has_ours {
            continue;
        }
        let hooks = root
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| invalid("hooks is not an object"))?;
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| invalid("a hooks event is not an array"))?;
        groups.push(json!({"hooks": [{"type": "command", "command": command}]}));
        changed = true;
    }
    if !changed {
        return Ok(HookStatus::Unchanged);
    }
    save(&path, &settings)?;
    Ok(HookStatus::Installed)
}

/// Where hooks go; None skips that target (app not found, or AH_NO_HOOKS=1).
#[derive(Debug, Default, Clone)]
pub struct HookDirs {
    pub claude: Option<PathBuf>,
    pub cursor: Option<PathBuf>,
}

impl HookDirs {
    pub fn detect() -> Self {
        Self {
            claude: claude_dir(),
            cursor: cursor_dir(),
        }
    }

    /// `detect` against a given home: `.claude` when it exists or `claude` is on PATH,
    /// `.cursor` when it exists, neither when opted out.
    pub fn detect_in(home: &Path, claude_on_path: bool, opt_out: bool) -> Self {
        let cursor = home.join(".cursor");
        Self {
            claude: detect(home.join(".claude"), claude_on_path, opt_out),
            cursor: (cursor.is_dir() && !opt_out).then_some(cursor),
        }
    }
}

pub const NO_HOOKS: &str = "no editor hooks installed; to stay reachable, this agent must listen: run `ah wait` in the background after each turn (harnesses that wake on background-command completion), or loop the wait_for_message MCP tool while idle.";

/// Ensures every detected target for the auth and join call sites and says how the agent stays
/// reachable: one line per target, or the manual listening rule when none applies. Install
/// failures are warnings, since login and binding must not fail over hooks.
pub fn ensure_and_report(dirs: &HookDirs, out: &mut impl Write) -> std::io::Result<()> {
    let targets = [
        (
            "claude code hooks",
            "ah hook stop/session-start",
            ensure_hooks(dirs.claude.as_deref()),
        ),
        (
            "cursor hook",
            "ah hook cursor-stop",
            ensure_cursor_hooks(dirs.cursor.as_deref()),
        ),
    ];
    let mut any = false;
    for (label, names, result) in targets {
        match result {
            Ok(HookStatus::Installed) => {
                any = true;
                writeln!(
                    out,
                    "{label} installed ({names}); remove with ah hooks uninstall"
                )?;
            }
            Ok(HookStatus::Unchanged) => {
                any = true;
                writeln!(out, "{label} present ({names})")?;
            }
            Ok(HookStatus::NotApplicable) => {}
            Err(e) => writeln!(out, "warning: could not install {label}: {e}")?,
        }
    }
    if !any {
        writeln!(out, "{NO_HOOKS}")?;
    }
    Ok(())
}

pub const CURSOR_STOP_CMD: &str = "ah hook cursor-stop";

/// `~/.cursor` when that directory exists and AH_NO_HOOKS is not 1.
pub fn cursor_dir() -> Option<PathBuf> {
    let off = opt_out(std::env::var_os("AH_NO_HOOKS").as_deref());
    HookDirs::detect_in(&dirs::home_dir()?, false, off).cursor
}

/// `~/.cursor` regardless of detection, for uninstall.
pub fn user_cursor_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".cursor"))
}

/// The running `ah` by absolute path: a Cursor started from the dock may not have `ah` on its
/// PATH. Falls back to bare `ah` when the path would not split cleanly (whitespace) or the
/// running binary is not `ah` (tests).
fn cursor_command() -> String {
    let exe = std::env::current_exe().ok();
    let full = exe
        .as_deref()
        .and_then(Path::to_str)
        .filter(|p| !p.contains(char::is_whitespace))
        .map(|p| format!("{p} hook cursor-stop"));
    match full {
        Some(c) if is_ours(&json!({ "command": c })) => c,
        _ => CURSOR_STOP_CMD.to_string(),
    }
}

fn cursor_stops(doc: &Value) -> &[Value] {
    doc["hooks"]["stop"].as_array().map_or(&[], Vec::as_slice)
}

/// Adds our entry to `{cursor_dir}/hooks.json` (`{"version": 1, "hooks": {"stop": [{"command":
/// ...}]}}`, cursor.com/docs/agent/hooks "Configuration file") unless a stop command passes
/// `is_ours`. Adds `version` only when missing. Never removes or reorders anything; leaves the
/// file untouched when nothing is missing or when it does not parse.
pub fn ensure_cursor_hooks(cursor_dir: Option<&Path>) -> std::io::Result<HookStatus> {
    let Some(cursor_dir) = cursor_dir else {
        return Ok(HookStatus::NotApplicable);
    };
    let path = cursor_dir.join("hooks.json");
    let mut doc = load(&path)?;
    if cursor_stops(&doc).iter().any(is_ours) {
        return Ok(HookStatus::Unchanged);
    }
    let root = doc
        .as_object_mut()
        .ok_or_else(|| invalid("not an object"))?;
    root.entry("version").or_insert(json!(1));
    let stop = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid("hooks is not an object"))?
        .entry("stop")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| invalid("hooks.stop is not an array"))?;
    stop.push(json!({ "command": cursor_command() }));
    save(&path, &doc)?;
    Ok(HookStatus::Installed)
}

/// Removes every `ah hook ...` command from `{cursor_dir}/hooks.json`, plus the event lists and
/// `hooks` object this leaves empty. Returns the removed commands.
pub fn uninstall_cursor(cursor_dir: Option<&Path>) -> std::io::Result<Vec<String>> {
    let Some(cursor_dir) = cursor_dir else {
        return Ok(Vec::new());
    };
    let path = cursor_dir.join("hooks.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut doc = load(&path)?;
    let mut removed = Vec::new();
    let Some(root) = doc.as_object_mut() else {
        return Err(invalid("not an object"));
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(removed);
    };
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(list) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        let before = removed.len();
        list.retain(|h| {
            let ours = is_ours(h);
            if ours {
                removed.extend(h["command"].as_str().map(String::from));
            }
            !ours
        });
        if list.is_empty() && removed.len() > before {
            hooks.shift_remove(&event);
        }
    }
    if removed.is_empty() {
        return Ok(removed);
    }
    if hooks.is_empty() {
        root.shift_remove("hooks");
    }
    save(&path, &doc)?;
    Ok(removed)
}

/// Cursor install state for `ah hooks status`.
pub fn cursor_status(cursor_dir: Option<&Path>) -> std::io::Result<&'static str> {
    let Some(cursor_dir) = cursor_dir else {
        return Ok("not applicable (~/.cursor not found, or AH_NO_HOOKS=1)");
    };
    let doc = load(&cursor_dir.join("hooks.json"))?;
    let present = cursor_stops(&doc).iter().any(is_ours);
    Ok(if present {
        "installed"
    } else {
        "not installed"
    })
}

/// Removes every `ah hook ...` command (see `is_ours`), plus the groups, event lists and
/// `hooks` object that this leaves empty. Returns the removed commands.
pub fn uninstall(claude_dir: Option<&Path>) -> std::io::Result<Vec<String>> {
    let Some(claude_dir) = claude_dir else {
        return Ok(Vec::new());
    };
    let path = claude_dir.join("settings.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut settings = load(&path)?;
    let mut removed = Vec::new();
    let Some(root) = settings.as_object_mut() else {
        return Err(invalid("not an object"));
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(removed);
    };
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(groups) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        let before = removed.len();
        groups.retain_mut(|group| {
            let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let had = list.len();
            list.retain(|h| {
                let ours = is_ours(h);
                if ours {
                    removed.extend(h["command"].as_str().map(String::from));
                }
                !ours
            });
            !(list.is_empty() && had > 0)
        });
        if groups.is_empty() && removed.len() > before {
            hooks.shift_remove(&event);
        }
    }
    if removed.is_empty() {
        return Ok(removed);
    }
    if hooks.is_empty() {
        root.shift_remove("hooks");
    }
    save(&path, &settings)?;
    Ok(removed)
}

/// Human-readable install state for `ah hooks status`.
pub fn status(claude_dir: Option<&Path>) -> std::io::Result<&'static str> {
    let Some(claude_dir) = claude_dir else {
        return Ok("not applicable (Claude Code not found, or AH_NO_HOOKS=1)");
    };
    let settings = load(&claude_dir.join("settings.json"))?;
    let present = EVENTS
        .iter()
        .filter(|(event, _)| {
            settings["hooks"][*event]
                .as_array()
                .is_some_and(|g| g.iter().flat_map(group_hooks).any(is_ours))
        })
        .count();
    Ok(match present {
        2 => "installed",
        0 => "not installed",
        _ => "partially installed; ah auth login or ah channel join repairs it",
    })
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct HookInput {
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub stop_hook_active: bool,
    /// Cursor sends the workspace folders instead of `cwd`.
    #[serde(default)]
    pub workspace_roots: Vec<PathBuf>,
}

#[derive(Debug, PartialEq)]
pub enum StopDecision {
    Allow,
    Block,
}

/// Blocks only a first stop in a bound folder with no live `ah wait`. A stop that is already
/// a hook-forced continuation is let through: one nudge per stop.
pub fn stop_decision(bound: bool, alive: bool, stop_hook_active: bool) -> StopDecision {
    if bound && !alive && !stop_hook_active {
        StopDecision::Block
    } else {
        StopDecision::Allow
    }
}

/// The folder's canonical key and bound channel ids; None when unbound or unreadable.
fn bound_channels(dir: &Path, input: &HookInput) -> Option<(String, Vec<Uuid>)> {
    let cwd = match &input.cwd {
        Some(c) => c.clone(),
        None => std::env::current_dir().ok()?,
    };
    bound_at(dir, &cwd)
}

fn bound_at(dir: &Path, cwd: &Path) -> Option<(String, Vec<Uuid>)> {
    let key = Bindings::key(cwd).ok()?;
    let ids = Bindings::load(dir).ok()?.0.remove(&key)?.channel_ids;
    (!ids.is_empty()).then_some((key, ids))
}

/// Channel names for `ids`, falling back to the id when the hub cannot be asked in time.
/// The flag says whether the hub answered.
async fn channel_names(client: Option<&HubClient>, ids: &[Uuid]) -> (Vec<(Uuid, String)>, bool) {
    let listed = match client {
        Some(c) => tokio::time::timeout(NET_TIMEOUT, c.list_channels())
            .await
            .ok()
            .and_then(Result::ok),
        None => None,
    };
    let reachable = listed.is_some();
    let listed = listed.unwrap_or_default();
    let names = ids
        .iter()
        .map(|id| {
            let name = listed.iter().find(|c| c.id == *id);
            (*id, name.map_or_else(|| id.to_string(), |c| c.name.clone()))
        })
        .collect();
    (names, reachable)
}

/// `ah hook stop`. Writes nothing unless the stop must be blocked.
pub async fn run_stop(
    dir: &Path,
    client: Option<&HubClient>,
    input: &HookInput,
    out: &mut impl Write,
) -> std::io::Result<()> {
    let bound = bound_channels(dir, input);
    let alive = bound
        .as_ref()
        .is_some_and(|(key, _)| listener_alive(dir, key));
    let decision = stop_decision(bound.is_some(), alive, input.stop_hook_active);
    let (StopDecision::Block, Some((_, ids))) = (decision, bound) else {
        return Ok(());
    };
    let (named, _) = channel_names(client, &ids).await;
    let names: Vec<String> = named.into_iter().map(|(_, n)| n).collect();
    let reason = format!(
        "This folder is in Agent Hub channel(s) {}. Start the listener before stopping: run `ah wait` as a background Bash command (run_in_background). It wakes you when a teammate messages; announce each as: {ANNOUNCE}.",
        names.join(", ")
    );
    writeln!(out, "{}", json!({"decision": "block", "reason": reason}))
}

/// `ah hook session-start`. Lists the folder's channels with unread counts (one GET each,
/// skipped on any network failure) and the listening rule. Silent when unbound.
pub async fn run_session_start(
    dir: &Path,
    client: Option<&HubClient>,
    input: &HookInput,
    out: &mut impl Write,
) -> std::io::Result<()> {
    let Some((key, ids)) = bound_channels(dir, input) else {
        return Ok(());
    };
    let cursors = Cursors::load_folder(dir, &key).unwrap_or_default();
    let mut parts = Vec::new();
    let (named, reachable) = channel_names(client, &ids).await;
    for (id, name) in named {
        // An unreachable hub already cost one timeout; do not pay another per channel.
        let unread = match (client.filter(|_| reachable), cursors.get(&id)) {
            (Some(c), Some(since)) => unread_count(c, id, *since).await,
            _ => None,
        };
        parts.push(match unread {
            Some(n) => format!("#{name} ({n} unread)"),
            None => format!("#{name}"),
        });
    }
    let context = format!(
        "This folder ({key}) is in Agent Hub channel(s): {}. Start listening now: run `ah wait` as a background Bash command (run_in_background). It exits when a teammate messages; announce each message to the user as: {ANNOUNCE}. Then answer it and start `ah wait` again.",
        parts.join(", ")
    );
    let v = json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": context}});
    writeln!(out, "{v}")
}

/// Messages after the cursor, from one page of up to 500; a full page means at least that many.
async fn unread_count(client: &HubClient, channel: Uuid, since: i64) -> Option<usize> {
    let page = tokio::time::timeout(NET_TIMEOUT, client.messages(channel, since, 500)).await;
    page.ok()?.ok().map(|m| m.len())
}

pub const CURSOR_FOLLOWUP_CLOSE: &str = "Handle these, reply with send_message, and keep the wait_for_message loop running while you are idle.";

/// The Cursor follow-up for messages fetched past the folder's cursors, plus the cursors to
/// save: per channel, the highest fetched id, own messages included (as `ah wait` does).
/// The payload is None when every fetched message is the agent's own, or nothing was fetched.
pub fn cursor_followup(
    fetched: &[(String, Vec<Message>)],
    is_self: impl Fn(&Message) -> bool,
) -> (Option<Value>, BTreeMap<Uuid, i64>) {
    let mut cursors = BTreeMap::new();
    let mut lines = Vec::new();
    for (channel, messages) in fetched {
        for m in messages {
            let slot = cursors.entry(m.channel_id).or_insert(m.id);
            *slot = (*slot).max(m.id);
            if !is_self(m) {
                lines.push(announce_line(&m.sender_name, channel, &m.body));
            }
        }
    }
    if lines.is_empty() {
        return (None, cursors);
    }
    lines.push(CURSOR_FOLLOWUP_CLOSE.to_string());
    (Some(json!({"followup_message": lines.join("\n")})), cursors)
}

/// The first workspace root bound to a channel, with its canonical key and channel ids.
fn cursor_bound(dir: &Path, input: &HookInput) -> Option<(PathBuf, String, Vec<Uuid>)> {
    input
        .workspace_roots
        .iter()
        .find_map(|root| bound_at(dir, root).map(|(key, ids)| (root.clone(), key, ids)))
}

/// `ah hook cursor-stop` (contract: cursor.com/docs/agent/hooks, "Hook events > stop").
/// Prints `{"followup_message": ...}`, which Cursor submits as the next user message, when a
/// bound workspace has messages from others past the shared cursor; otherwise prints nothing.
///
/// Loop guard: the cursor moves past every announced message before the next stop can run,
/// and only messages past the cursor are announced. A stop with nothing new is silent, so a
/// quiet stop is terminal and the follow-up chain ends once the channel goes quiet. Cursor's
/// default `loop_limit` (5 follow-ups per script) caps a busy channel on top of that.
pub async fn run_cursor_stop(
    dir: &Path,
    client: Option<&HubClient>,
    input: &HookInput,
    out: &mut impl Write,
) -> std::io::Result<()> {
    let (Some((root, key, ids)), Some(client)) = (cursor_bound(dir, input), client) else {
        return Ok(());
    };
    let listed = tokio::time::timeout(NET_TIMEOUT, client.list_channels()).await;
    let Ok(Ok(listed)) = listed else {
        return Ok(());
    };
    let Ok(known) = Cursors::load_folder(dir, &key) else {
        return Ok(());
    };
    let mut seeds = BTreeMap::new();
    let mut fetched = Vec::new();
    for ch in listed.iter().filter(|c| ids.contains(&c.id)) {
        let Some(&since) = known.get(&ch.id) else {
            // First run: history is onboarding's job, so start at the current tail (as ah wait).
            seeds.insert(ch.id, ch.last_message_id);
            continue;
        };
        let page = tokio::time::timeout(NET_TIMEOUT, fetch_since(client, ch.id, since)).await;
        let Ok(Ok(messages)) = page else {
            return Ok(());
        };
        fetched.push((ch.name.clone(), messages));
    }
    // ponytail: self filter by name, as ah wait. The hook process may not see CURSOR_AGENT
    // the way the MCP server does, so both the detected and the "cursor" name count as self.
    let own = [default_agent_name(&root), agent_name("cursor", &root)];
    let (payload, mut cursors) = cursor_followup(&fetched, |m| own.contains(&m.sender_name));
    cursors.extend(seeds);
    if let Some(payload) = payload {
        writeln!(out, "{payload}")?;
        out.flush()?;
    }
    // Saved once, after the follow-up is out: a failed print re-announces next stop rather
    // than losing the messages.
    if !cursors.is_empty() {
        Cursors::advance(dir, &key, &cursors)?;
    }
    Ok(())
}

pub enum Event {
    Stop,
    SessionStart,
    CursorStop,
}

/// Entry point for `ah hook <event>`. Never fails: a hook error would show in every Claude
/// Code session on the machine, so every problem means "no output".
pub async fn run_hook(dir: &Path, event: Event) {
    use std::io::{IsTerminal, Read};
    let mut raw = String::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let _ = stdin.lock().read_to_string(&mut raw);
    }
    let input: HookInput = serde_json::from_str(&raw).unwrap_or_default();
    let client = CliConfig::load(dir).ok().and_then(|cfg| {
        let creds = Credentials::load(dir).ok().flatten()?;
        Some(HubClient::new(cfg.hub_url, Some(creds.token)))
    });
    let mut buf = Vec::new();
    let ran = match event {
        Event::Stop => run_stop(dir, client.as_ref(), &input, &mut buf).await,
        Event::SessionStart => run_session_start(dir, client.as_ref(), &input, &mut buf).await,
        // Writes straight to stdout: it saves the cursor only after the follow-up is flushed.
        Event::CursorStop => {
            let mut input = input;
            // User hooks run from ~/.cursor, so the process cwd is never the workspace. The env
            // root is a last candidate in case no workspace_roots entry resolves.
            input
                .workspace_roots
                .extend(std::env::var_os("CURSOR_PROJECT_DIR").map(PathBuf::from));
            let mut stdout = std::io::stdout();
            let _ = run_cursor_stop(dir, client.as_ref(), &input, &mut stdout).await;
            std::process::exit(0);
        }
    };
    if ran.is_ok() {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(&buf);
        let _ = stdout.flush();
    }
    // A timed-out request can leave a blocking DNS lookup that runtime shutdown would wait
    // for; exit now so the hook never outlives its decision.
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_decision_matrix() {
        for bound in [false, true] {
            for alive in [false, true] {
                for active in [false, true] {
                    let want = if bound && !alive && !active {
                        StopDecision::Block
                    } else {
                        StopDecision::Allow
                    };
                    assert_eq!(
                        stop_decision(bound, alive, active),
                        want,
                        "bound={bound} alive={alive} active={active}"
                    );
                }
            }
        }
    }

    fn msg(id: i64, channel: Uuid, sender: &str, body: &str) -> Message {
        Message {
            id,
            channel_id: channel,
            sender_type: ah_common::SenderType::Agent,
            sender_name: sender.into(),
            body: body.into(),
            reply_to: None,
            mentions: vec![],
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn cursor_followup_silent_without_new_messages() {
        let (payload, cursors) = cursor_followup(&[], |_| false);
        assert!(payload.is_none());
        assert!(cursors.is_empty());
        let (payload, cursors) = cursor_followup(&[("team".into(), vec![])], |_| false);
        assert!(payload.is_none());
        assert!(cursors.is_empty());
    }

    #[test]
    fn cursor_followup_announces_and_advances_to_max() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let long = "x".repeat(250);
        let fetched = vec![
            (
                "team".to_string(),
                vec![msg(4, a, "ann", "hi\nthere"), msg(7, a, "bob", &long)],
            ),
            ("ops".to_string(), vec![msg(12, b, "cat", "deploy?")]),
        ];
        let (payload, cursors) = cursor_followup(&fetched, |_| false);
        let payload = payload.unwrap();
        // Contract: exactly one key, a non-empty string.
        assert_eq!(payload.as_object().unwrap().len(), 1);
        let text = payload["followup_message"].as_str().unwrap();
        let want = format!(
            "new message from ann in #team: hi there\nnew message from bob in #team: {}...\nnew message from cat in #ops: deploy?\n{CURSOR_FOLLOWUP_CLOSE}",
            "x".repeat(200)
        );
        assert_eq!(text, want);
        assert_eq!(cursors, BTreeMap::from([(a, 7), (b, 12)]));
    }

    #[test]
    fn cursor_followup_skips_own_messages_but_moves_past_them() {
        let a = Uuid::new_v4();
        let me = |m: &Message| m.sender_name == "me";
        let (payload, cursors) =
            cursor_followup(&[("team".into(), vec![msg(3, a, "me", "x")])], me);
        assert!(payload.is_none());
        assert_eq!(cursors, BTreeMap::from([(a, 3)]));

        let fetched = vec![(
            "team".to_string(),
            vec![msg(5, a, "ann", "q"), msg(6, a, "me", "answer")],
        )];
        let (payload, cursors) = cursor_followup(&fetched, me);
        let text = payload.unwrap()["followup_message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            text.starts_with("new message from ann in #team: q\n"),
            "{text}"
        );
        assert!(!text.contains("answer"), "{text}");
        assert_eq!(cursors, BTreeMap::from([(a, 6)]));
    }
}
