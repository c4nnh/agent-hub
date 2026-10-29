use ah::{commands::auth, config};
use clap::{Parser, Subcommand};

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
    }
}
