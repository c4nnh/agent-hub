//! Claude Code hooks that keep a channel-bound folder listening.
//!
//! Contract (code.claude.com/docs/en/hooks, "Stop input", "Stop decision control",
//! "SessionStart"): the hook gets a JSON object on stdin with `cwd` and, for Stop,
//! `stop_hook_active`. On exit 0, one JSON object on stdout is parsed: Stop blocks with
//! `{"decision":"block","reason":...}`; SessionStart adds
//! `hookSpecificOutput.additionalContext`. No output means no effect.

use crate::{
    bindings::Bindings,
    client::HubClient,
    config::{CliConfig, Credentials},
    cursors::{Cursors, listener_alive},
};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

pub const STOP_CMD: &str = "ah hook stop";
pub const SESSION_START_CMD: &str = "ah hook session-start";
/// Any hook command containing this is ours, wherever `ah` lives.
const MARKER: &str = "ah hook ";
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
        format!("settings.json: {msg}"),
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

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains(MARKER))
}

fn group_hooks(group: &Value) -> &[Value] {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Adds the two matcher-less entries to `{claude_dir}/settings.json` where no `ah hook `
/// command exists yet. Never removes or reorders anything; leaves the file untouched when
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

/// `ensure_hooks` for the auth and join call sites: one line when something was installed,
/// a warning instead of an error, since login and binding must not fail over hooks.
pub fn ensure_and_report(claude_dir: Option<&Path>, out: &mut impl Write) -> std::io::Result<()> {
    match ensure_hooks(claude_dir) {
        Ok(HookStatus::Installed) => writeln!(
            out,
            "claude code hooks installed (ah hook stop/session-start); remove with ah hooks uninstall"
        ),
        Ok(_) => Ok(()),
        Err(e) => writeln!(out, "warning: could not install claude code hooks: {e}"),
    }
}

/// Removes every hook whose command contains `ah hook `, plus the groups, event lists and
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
    let key = Bindings::key(&cwd).ok()?;
    let ids = Bindings::load(dir).ok()?.0.remove(&key)?.channel_ids;
    (!ids.is_empty()).then_some((key, ids))
}

/// Channel names for `ids`, falling back to the id when the hub cannot be asked in time.
async fn channel_names(client: Option<&HubClient>, ids: &[Uuid]) -> Vec<(Uuid, String)> {
    let listed = match client {
        Some(c) => tokio::time::timeout(NET_TIMEOUT, c.list_channels())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
        None => Vec::new(),
    };
    ids.iter()
        .map(|id| {
            let name = listed.iter().find(|c| c.id == *id);
            (*id, name.map_or_else(|| id.to_string(), |c| c.name.clone()))
        })
        .collect()
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
    let names: Vec<String> = channel_names(client, &ids)
        .await
        .into_iter()
        .map(|(_, n)| n)
        .collect();
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
    for (id, name) in channel_names(client, &ids).await {
        let unread = match (client, cursors.get(&id)) {
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

pub enum Event {
    Stop,
    SessionStart,
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
    };
    if ran.is_ok() {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(&buf);
        let _ = stdout.flush();
    }
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
}
