use ah::{
    client::HubClient,
    commands::{auth, channel, chat, update, wait},
    config::{self, Credentials},
};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;

#[derive(Parser)]
#[command(name = "ah", version, about = "Agent Hub CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log in through the browser
    Auth {
        /// Print the login URL instead of opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Revoke the token and delete local credentials
    Logout,
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
    /// Update ah to the latest release
    Update {
        /// Install this release tag (e.g. v0.1.2) instead of the latest
        #[arg(long)]
        version: Option<String>,
    },
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
    },
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
    /// Leave a channel and drop its folder bindings
    Leave {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Invite someone by email
    Invite { channel: String, email: String },
    /// Pending invitations
    Invitation {
        #[command(subcommand)]
        command: InvitationCommand,
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
    let result = run(cli, &dir).await;
    if let Err(e) = result {
        eprintln!("error: {e}");
        // 2 = the feed is refused or the folder is unbound; retrying will not help.
        let is_refusal = e.downcast_ref::<wait::Refusal>().is_some();
        std::process::exit(if is_refusal { 2 } else { 1 });
    }
}

async fn run(cli: Cli, dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = config::CliConfig::load(dir)?;
    let mut out = std::io::stdout();
    match cli.command {
        Command::Auth { no_browser } => {
            auth::run_auth(
                dir,
                &cfg,
                no_browser,
                auth::POLL_INTERVAL,
                auth::POLL_TIMEOUT,
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
        Command::Mcp { name, folder } => ah::mcp::run(dir, &cfg, name, folder).await,
        Command::Wait {
            channel,
            folder,
            name,
        } => {
            let creds = Credentials::load(dir)?.ok_or("not logged in; run `ah auth`")?;
            let client = HubClient::new(cfg.hub_url.clone(), Some(creds.token));
            let args = wait::WaitArgs {
                folder,
                channel,
                name,
                ..Default::default()
            };
            wait::run_wait(dir, &client, args, &mut out)
                .await
                .map_err(|e| e as Box<dyn std::error::Error>)
        }
        Command::Logout => auth::run_logout(dir, &cfg, &mut out, &mut std::io::stderr()).await,
        Command::Chat { channel } => {
            let creds = Credentials::load(dir)?.ok_or("not logged in; run `ah auth`")?;
            let client = HubClient::new(cfg.hub_url.clone(), Some(creds.token));
            let input = tokio::io::BufReader::new(tokio::io::stdin());
            chat::run_chat(&client, &channel, input, &mut out).await
        }
        Command::Channel { command } => {
            let creds = Credentials::load(dir)?.ok_or("not logged in; run `ah auth`")?;
            let client = HubClient::new(cfg.hub_url.clone(), Some(creds.token));
            let cwd = std::env::current_dir()?;
            match command {
                ChannelCommand::Create { name } => {
                    channel::run_create(&client, &name, &mut out).await
                }
                ChannelCommand::Join { channel } => {
                    channel::run_join(dir, &client, &cwd, channel.as_deref(), &mut out).await
                }
                ChannelCommand::Leave { channel, yes } => {
                    channel::run_leave(dir, &client, channel.as_deref(), yes, &mut out).await
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
}
