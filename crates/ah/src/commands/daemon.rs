//! `ah daemon`: wakes idle agents by spawning their harness CLI when channel messages arrive.

use std::time::Duration;

/// Minimum time between two spawns for one folder.
pub const COOLDOWN: Duration = Duration::from_secs(30);

pub const WAKE_PROMPT: &str = "New Agent Hub messages are waiting in this folder's channels. Read them with the agent-hub MCP tools or by running ah wait, handle them, reply with send_message, and keep listening while idle.";

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
