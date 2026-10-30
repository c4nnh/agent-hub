use ah::{
    client::HubClient,
    commands::{auth, channel, chat, daemon, guide, update, wait},
    config::{self, Credentials},
    hooks,
};
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Write};

#[derive(Parser)]
#[command(name = "ah", version, about = "Agent Hub CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log in or out
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Claude Code and Cursor hook entry points (run by the hooks that `ah auth login` and
    /// `ah channel join` install)
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },
    /// Inspect or remove the installed Claude Code and Cursor hooks
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
    /// Manage channels and folder bindings
    Channel {
        #[command(subcommand)]
        command: ChannelCommand,
    },
    /// Chat live in a channel (Ctrl-D to exit)
    Chat { channel: String },
    /// Run the MCP server for a coding agent (stdio)
    Mcp {
        /// Agent name shown in channels; default `<harness>@<hostname>/<folder name>`
        #[arg(long)]
        name: Option<String>,
        /// Folder to look up the channel binding for; default the current directory
        #[arg(long)]
        folder: Option<std::path::PathBuf>,
    },
    /// Print the agent protocol: how to stay reachable and answer in real time
    Guide,
    /// Update ah to the latest release
    Update {
        /// Install this release tag (e.g. v0.1.2) instead of the latest
        #[arg(long)]
        version: Option<String>,
    },
    /// Run in the foreground and wake idle agents (folders with `ah channel wake` set) when
    /// messages arrive and nothing is listening
    Daemon,
    /// Block until a message from someone else arrives; print it as JSON lines and exit
    Wait {
        /// Only wait on this channel (default: every channel bound to the folder)
        #[arg(long)]
        channel: Option<String>,
        /// Folder to look up the channel binding for; default the current directory
        #[arg(long)]
        folder: Option<std::path::PathBuf>,
        /// Agent name whose own messages are ignored; default as for `ah mcp`
        #[arg(long)]
        name: Option<String>,
        /// Run even under Cursor, where a background wait swallows messages
        #[arg(long)]
        allow_cursor: bool,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Log in through the browser
    Login {
        /// Print the login URL instead of opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Revoke the token and delete local credentials
    Logout,
}

#[derive(Subcommand)]
enum HookEvent {
    /// Stop hook: blocks the stop when a bound folder has no running `ah wait`
    Stop,
    /// SessionStart hook: lists the folder's channels and unread counts
    SessionStart,
    /// Cursor stop hook: hands pending messages to the agent as a follow-up message
    CursorStop,
}

#[derive(Subcommand)]
enum HooksCommand {
    /// Remove the `ah hook` entries from ~/.claude/settings.json and ~/.cursor/hooks.json
    Uninstall,
    /// Show whether the hooks are installed
    Status,
}

#[derive(Subcommand)]
enum ChannelCommand {
    /// Create a channel
    Create { name: String },
    /// Bind the current folder to a channel
    Join {
        #[arg(long)]
        channel: Option<String>,
    },
    /// Unbind the current folder from a channel; with --account, leave the channel itself
    Leave {
        #[arg(long)]
        channel: Option<String>,
        /// Folder to unbind; default the current directory
        #[arg(long, conflicts_with = "account")]
        folder: Option<std::path::PathBuf>,
        /// Leave the channel on the hub and drop it from every folder binding
        #[arg(long)]
        account: bool,
        /// Skip the confirmation for --account
        #[arg(long, requires = "account")]
        yes: bool,
    },
    /// Invite someone by email
    Invite { channel: String, email: String },
    /// Pending invitations
    Invitation {
        #[command(subcommand)]
        command: InvitationCommand,
    },
    /// Show, set or remove the command `ah daemon` runs to wake this folder's idle agent
    Wake {
        /// Folder to configure; default the current directory
        #[arg(long)]
        folder: Option<std::path::PathBuf>,
        /// Use the known command for a harness
        #[arg(long, value_parser = ["cursor", "codex", "claude-code"], conflicts_with_all = ["command", "off"])]
        harness: Option<String>,
        /// Custom command template; `{prompt}` is replaced with the wake prompt
        #[arg(long, conflicts_with = "off")]
        command: Option<String>,
        /// Remove the wake command
        #[arg(long)]
        off: bool,
    },
    /// Show or edit the current folder's binding
    Config {
        #[arg(long)]
        folder: Option<std::path::PathBuf>,
        #[arg(long)]
        add_extra_folder: Option<std::path::PathBuf>,
        #[arg(long)]
        remove_extra_folder: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum InvitationCommand {
    /// List pending invitations; answer with --respond, or interactively
    List {
        #[arg(long, requires = "answer")]
        respond: Option<uuid::Uuid>,
        #[command(flatten)]
        answer: Answer,
    },
}

#[derive(clap::Args)]
#[group(id = "answer", multiple = false)]
struct Answer {
    #[arg(long)]
    accept: bool,
    #[arg(long)]
    reject: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let dir = config::config_dir();
    // Hooks run in every Claude Code session: they must not fail on a bad config.
    if let Command::Hook { event } = cli.command {
        let event = match event {
            HookEvent::Stop => hooks::Event::Stop,
            HookEvent::SessionStart => hooks::Event::SessionStart,
            HookEvent::CursorStop => hooks::Event::CursorStop,
        };
        hooks::run_hook(&dir, event).await;
        return;
    }
    let result = run(cli, &dir).await;
    if let Err(e) = result {
        eprintln!("error: {e}");
        // 2 = the feed is refused or the folder is unbound; retrying will not help.
        let is_refusal = e.downcast_ref::<wait::Refusal>().is_some();
        std::process::exit(if is_refusal { 2 } else { 1 });
    }
}

fn run_hooks(
    command: HooksCommand,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        HooksCommand::Uninstall => {
            let mut removed = hooks::uninstall(hooks::user_claude_dir().as_deref())?;
            removed.extend(hooks::uninstall_cursor(
                hooks::user_cursor_dir().as_deref(),
            )?);
            let claude = hooks::user_claude_dir().map(|d| hooks::claude_mcp_file(&d));
            let cursor = hooks::user_cursor_dir().map(|d| hooks::cursor_mcp_file(&d));
            let mcp = [
                ("claude code", hooks::uninstall_mcp(claude.as_deref())?),
                ("cursor", hooks::uninstall_mcp(cursor.as_deref())?),
            ];
            let removed_mcp = mcp.iter().any(|(_, removed)| *removed);
            if removed.is_empty() && !removed_mcp {
                writeln!(out, "no ah hook or mcp entries found")?;
            }
            for c in removed {
                writeln!(out, "removed hook: {c}")?;
            }
            for (label, _) in mcp.iter().filter(|(_, removed)| *removed) {
                writeln!(out, "removed mcp server: {label}")?;
            }
        }
        HooksCommand::Status => {
            let status = hooks::status(hooks::claude_dir().as_deref())?;
            writeln!(out, "claude code hooks: {status}")?;
            let status = hooks::cursor_status(hooks::cursor_dir().as_deref())?;
            writeln!(out, "cursor hooks: {status}")?;
            let claude = hooks::claude_dir().map(|d| hooks::claude_mcp_file(&d));
            let status = hooks::mcp_status(claude.as_deref())?;
            writeln!(out, "claude code mcp: {status}")?;
            let cursor = hooks::cursor_dir().map(|d| hooks::cursor_mcp_file(&d));
            let status = hooks::mcp_status(cursor.as_deref())?;
            writeln!(out, "cursor mcp: {status}")?;
        }
    }
    Ok(())
}

async fn run(cli: Cli, dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = std::io::stdout();
    // Local settings only: works with a missing or broken CLI config.
    let command = match cli.command {
        Command::Hooks { command } => return run_hooks(command, &mut out),
        other => other,
    };
    let cfg = config::CliConfig::load(dir)?;
    match command {
        Command::Auth {
            command: AuthCommand::Login { no_browser },
        } => {
            auth::run_auth(
                dir,
                &cfg,
                no_browser,
                auth::POLL_INTERVAL,
                auth::POLL_TIMEOUT,
                &hooks::HookDirs::detect(),
                &mut out,
            )
            .await
        }
        Command::Update { version } => {
            let api_base = std::env::var("AH_UPDATE_API_BASE")
                .unwrap_or_else(|_| update::DEFAULT_API_BASE.to_string());
            let dest = std::env::current_exe()?.canonicalize()?;
            update::run_update(&api_base, version.as_deref(), &dest, &mut out)
                .await
                .map_err(|e| e as Box<dyn std::error::Error>)
        }
        Command::Guide => Ok(guide::run_guide(guide::Harness::detect(), &mut out)?),
        Command::Hook { .. } | Command::Hooks { .. } => Ok(()),
        Command::Daemon => {
            let creds = Credentials::load(dir)?.ok_or("not logged in; run `ah auth login`")?;
            let client = HubClient::new(cfg.hub_url.clone(), Some(creds.token));
            daemon::run_daemon(dir, client)
                .await
                .map_err(|e| e as Box<dyn std::error::Error>)
        }
        Command::Mcp { name, folder } => ah::mcp::run(dir, &cfg, name, folder).await,
        Command::Wait {
            channel,
            folder,
            name,
            allow_cursor,
        } => {
            let creds = Credentials::load(dir)?.ok_or("not logged in; run `ah auth login`")?;
            let client = HubClient::new(cfg.hub_url.clone(), Some(creds.token));
            let args = wait::WaitArgs {
                folder,
                channel,
                name,
                cursor_env: std::env::var_os("CURSOR_AGENT").is_some(),
                allow_cursor,
                ..Default::default()
            };
            wait::run_wait(dir, &client, args, &mut out)
                .await
                .map_err(|e| e as Box<dyn std::error::Error>)
        }
        Command::Auth {
            command: AuthCommand::Logout,
        } => auth::run_logout(dir, &cfg, &mut out, &mut std::io::stderr()).await,
        Command::Chat { channel } => {
            let creds = Credentials::load(dir)?.ok_or("not logged in; run `ah auth login`")?;
            let client = HubClient::new(cfg.hub_url.clone(), Some(creds.token));
            let input = tokio::io::BufReader::new(tokio::io::stdin());
            chat::run_chat(&client, &channel, input, &mut out).await
        }
        Command::Channel { command } => {
            let creds = Credentials::load(dir)?.ok_or("not logged in; run `ah auth login`")?;
            let client = HubClient::new(cfg.hub_url.clone(), Some(creds.token));
            let cwd = std::env::current_dir()?;
            match command {
                ChannelCommand::Create { name } => {
                    channel::run_create(&client, &name, &mut out).await
                }
                ChannelCommand::Join { channel } => {
                    let targets = hooks::HookDirs::detect();
                    channel::run_join(
                        dir,
                        &client,
                        &cwd,
                        channel.as_deref(),
                        &targets,
                        guide::Harness::detect(),
                        &mut out,
                    )
                    .await
                }
                ChannelCommand::Leave {
                    channel,
                    account: true,
                    yes,
                    ..
                } => {
                    channel::run_leave_account(dir, &client, channel.as_deref(), yes, &mut out)
                        .await
                }
                ChannelCommand::Leave {
                    channel, folder, ..
                } => {
                    let folder = folder.unwrap_or_else(|| cwd.clone());
                    channel::run_leave(dir, &client, &folder, channel.as_deref(), &mut out).await
                }
                ChannelCommand::Invite { channel, email } => {
                    channel::run_invite(&client, &channel, &email, &mut out).await
                }
                ChannelCommand::Invitation {
                    command: InvitationCommand::List { respond, answer },
                } => match respond {
                    Some(id) => {
                        channel::run_invitation(&client, Some((id, answer.accept)), false, &mut out)
                            .await
                    }
                    None => {
                        let is_tty = std::io::stdin().is_terminal();
                        channel::run_invitation(&client, None, is_tty, &mut out).await
                    }
                },
                ChannelCommand::Wake {
                    folder,
                    harness,
                    command,
                    off,
                } => channel::run_wake(
                    dir,
                    &cwd,
                    channel::WakeArgs {
                        folder,
                        harness,
                        command,
                        off,
                    },
                    &mut out,
                ),
                ChannelCommand::Config {
                    folder,
                    add_extra_folder,
                    remove_extra_folder,
                } => channel::run_config(
                    dir,
                    &cwd,
                    channel::ConfigArgs {
                        folder,
                        add_extra_folder,
                        remove_extra_folder,
                    },
                    &mut out,
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "00000000-0000-0000-0000-000000000001";

    #[test]
    fn respond_requires_accept_or_reject() {
        let parse = |extra: &[&str]| {
            let mut a = vec!["ah", "channel", "invitation", "list"];
            a.extend_from_slice(extra);
            Cli::try_parse_from(a)
        };
        assert!(parse(&["--respond", ID, "--accept"]).is_ok());
        assert!(parse(&["--respond", ID, "--reject"]).is_ok());
        assert!(parse(&["--respond", ID]).is_err());
        assert!(parse(&["--respond", ID, "--accept", "--reject"]).is_err());
        assert!(parse(&[]).is_ok());
    }

    #[test]
    fn auth_and_leave_shapes() {
        let ok = |a: &[&str]| Cli::try_parse_from(a).is_ok();
        assert!(ok(&["ah", "auth", "login", "--no-browser"]));
        assert!(ok(&["ah", "auth", "logout"]));
        assert!(!ok(&["ah", "auth"]));
        assert!(!ok(&["ah", "logout"]));
        assert!(ok(&["ah", "channel", "leave", "--folder", "/tmp"]));
        assert!(ok(&["ah", "channel", "leave", "--account", "--yes"]));
        assert!(!ok(&["ah", "channel", "leave", "--yes"]));
        assert!(!ok(&[
            "ah",
            "channel",
            "leave",
            "--account",
            "--folder",
            "/tmp"
        ]));
    }

    #[test]
    fn wake_flags_conflict() {
        let ok = |a: &[&str]| Cli::try_parse_from(a).is_ok();
        assert!(ok(&["ah", "channel", "wake"]));
        assert!(ok(&["ah", "channel", "wake", "--harness", "cursor"]));
        assert!(ok(&["ah", "channel", "wake", "--command", "x {prompt}"]));
        assert!(ok(&["ah", "channel", "wake", "--off"]));
        assert!(!ok(&["ah", "channel", "wake", "--harness", "vim"]));
        assert!(!ok(&[
            "ah",
            "channel",
            "wake",
            "--harness",
            "codex",
            "--command",
            "x"
        ]));
        assert!(!ok(&["ah", "channel", "wake", "--off", "--command", "x"]));
    }
}
