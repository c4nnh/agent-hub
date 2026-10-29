use ah::{
    client::HubClient,
    commands::{auth, channel, chat},
    config::{self, Credentials},
};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;

#[derive(Parser)]
#[command(name = "ah", about = "Agent Hub CLI")]
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
        std::process::exit(1);
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
