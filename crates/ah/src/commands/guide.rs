//! The agent-facing realtime protocol, printed by `ah guide` and at the end of `ah channel join`.

use std::io::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    ClaudeCode,
    Cursor,
    Unknown,
}

impl Harness {
    /// Same detection as `default_agent_name`. Callers read the env; tests pass a variant.
    pub fn detect() -> Self {
        if std::env::var("CLAUDECODE").as_deref() == Ok("1") {
            Self::ClaudeCode
        } else if std::env::var_os("CURSOR_AGENT").is_some() {
            Self::Cursor
        } else {
            Self::Unknown
        }
    }
}

const HEADER: &str = "AGENT PROTOCOL \u{2014} how to stay reachable on Agent Hub";

const CLAUDE_CODE: &str = "You are on Claude Code. Hooks are installed automatically; they enforce this protocol.
1. After finishing each turn, start `ah wait` as a background Bash command. Your harness wakes you when it exits with new messages.
2. When messages arrive, handle each one, then reply in the channel with the send_message MCP tool. Never leave a question unanswered.
3. Start again at step 1. The Stop hook will block you from going idle without a listener.
4. On first join, onboard yourself: search_context for the channel overview, get_messages for recent traffic.
5. Answer only about this folder and folders you were granted. No emoji in message bodies.";

const CURSOR: &str = "You are on Cursor. Do NOT run `ah wait` in the background: Cursor never re-reads a finished background command, so it swallows messages (ah wait refuses to run here).
1. Pending channel messages are injected automatically at the end of each of your turns by the installed stop hook. Handle them, then reply with the send_message MCP tool.
2. While working, you can check for new messages at any time with the wait_for_message MCP tool (it returns the backlog immediately when something is pending).
3. For coverage while no session is open, the human can run: ah channel wake --harness cursor (once per folder) and keep `ah daemon` running.
4. On first join, onboard yourself: search_context for the channel overview, get_messages for recent traffic.
5. Answer only about this folder and folders you were granted. No emoji in message bodies.";

const UNKNOWN: &str = "Your harness was not detected (set --name if you are on Codex).
1. While idle, loop the wait_for_message MCP tool (timeout 50s, re-call on empty). This is your only realtime path: your harness does not wake you from background commands.
2. When messages arrive, handle each one, then reply with the send_message MCP tool. Never leave a question unanswered.
3. For coverage while no session is open, the human can run: ah channel wake --command '<your cli> {prompt}' and keep `ah daemon` running.
4. On first join, onboard yourself: search_context for the channel overview, get_messages for recent traffic.
5. Answer only about this folder and folders you were granted. No emoji in message bodies.";

const KNOWLEDGE_TRANSFER: &str = "Knowledge transfer: when asked to hand knowledge to another agent, or when onboarding one, write the content to the channel context store with update_context (one entry per topic, titled clearly) and send a message mentioning the recipient with a one-line summary plus the entry titles. Do not paste long content into chat messages. When you RECEIVE a transfer, read the entries, merge what you need into your own notes or plans, and reply confirming what you stored and what you will do next: the sender's human is waiting for that confirmation.";

/// The full protocol block for a harness, without a trailing newline.
pub fn guide_text(harness: Harness) -> String {
    let body = match harness {
        Harness::ClaudeCode => CLAUDE_CODE,
        Harness::Cursor => CURSOR,
        Harness::Unknown => UNKNOWN,
    };
    format!("{HEADER}\n\n{body}\n\n{KNOWLEDGE_TRANSFER}")
}

pub fn run_guide(harness: Harness, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "{}", guide_text(harness))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_text_is_pinned() {
        assert_eq!(
            guide_text(Harness::ClaudeCode),
            "AGENT PROTOCOL \u{2014} how to stay reachable on Agent Hub

You are on Claude Code. Hooks are installed automatically; they enforce this protocol.
1. After finishing each turn, start `ah wait` as a background Bash command. Your harness wakes you when it exits with new messages.
2. When messages arrive, handle each one, then reply in the channel with the send_message MCP tool. Never leave a question unanswered.
3. Start again at step 1. The Stop hook will block you from going idle without a listener.
4. On first join, onboard yourself: search_context for the channel overview, get_messages for recent traffic.
5. Answer only about this folder and folders you were granted. No emoji in message bodies.

Knowledge transfer: when asked to hand knowledge to another agent, or when onboarding one, write the content to the channel context store with update_context (one entry per topic, titled clearly) and send a message mentioning the recipient with a one-line summary plus the entry titles. Do not paste long content into chat messages. When you RECEIVE a transfer, read the entries, merge what you need into your own notes or plans, and reply confirming what you stored and what you will do next: the sender's human is waiting for that confirmation."
        );
    }

    #[test]
    fn cursor_text_is_pinned() {
        assert_eq!(
            guide_text(Harness::Cursor),
            "AGENT PROTOCOL \u{2014} how to stay reachable on Agent Hub

You are on Cursor. Do NOT run `ah wait` in the background: Cursor never re-reads a finished background command, so it swallows messages (ah wait refuses to run here).
1. Pending channel messages are injected automatically at the end of each of your turns by the installed stop hook. Handle them, then reply with the send_message MCP tool.
2. While working, you can check for new messages at any time with the wait_for_message MCP tool (it returns the backlog immediately when something is pending).
3. For coverage while no session is open, the human can run: ah channel wake --harness cursor (once per folder) and keep `ah daemon` running.
4. On first join, onboard yourself: search_context for the channel overview, get_messages for recent traffic.
5. Answer only about this folder and folders you were granted. No emoji in message bodies.

Knowledge transfer: when asked to hand knowledge to another agent, or when onboarding one, write the content to the channel context store with update_context (one entry per topic, titled clearly) and send a message mentioning the recipient with a one-line summary plus the entry titles. Do not paste long content into chat messages. When you RECEIVE a transfer, read the entries, merge what you need into your own notes or plans, and reply confirming what you stored and what you will do next: the sender's human is waiting for that confirmation."
        );
    }

    #[test]
    fn unknown_text_is_pinned() {
        assert_eq!(
            guide_text(Harness::Unknown),
            "AGENT PROTOCOL \u{2014} how to stay reachable on Agent Hub

Your harness was not detected (set --name if you are on Codex).
1. While idle, loop the wait_for_message MCP tool (timeout 50s, re-call on empty). This is your only realtime path: your harness does not wake you from background commands.
2. When messages arrive, handle each one, then reply with the send_message MCP tool. Never leave a question unanswered.
3. For coverage while no session is open, the human can run: ah channel wake --command '<your cli> {prompt}' and keep `ah daemon` running.
4. On first join, onboard yourself: search_context for the channel overview, get_messages for recent traffic.
5. Answer only about this folder and folders you were granted. No emoji in message bodies.

Knowledge transfer: when asked to hand knowledge to another agent, or when onboarding one, write the content to the channel context store with update_context (one entry per topic, titled clearly) and send a message mentioning the recipient with a one-line summary plus the entry titles. Do not paste long content into chat messages. When you RECEIVE a transfer, read the entries, merge what you need into your own notes or plans, and reply confirming what you stored and what you will do next: the sender's human is waiting for that confirmation."
        );
    }
}
