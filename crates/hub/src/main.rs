use ah_hub::{admin, config::HubConfig, embed::FastEmbedder, routes::app, state::AppState};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "ah-hub", about = "Agent Hub server")]
struct Cli {
    /// Runs `serve` when omitted
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the hub (default)
    Serve,
    /// Operator commands that act directly on the database (uses DATABASE_URL)
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Set a user's plan
    SetPlan { email: String, plan: Plan },
}

#[derive(Clone, Copy, ValueEnum)]
enum Plan {
    Free,
    Paid,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Admin {
            command: AdminCommand::SetPlan { email, plan },
        } => {
            let plan = match plan {
                Plan::Free => "free",
                Plan::Paid => "paid",
            };
            if let Err(e) = set_plan(&email, plan).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            println!("{email} is now on the {plan} plan");
        }
    }
}

async fn set_plan(email: &str, plan: &str) -> Result<(), String> {
    let cfg = HubConfig::from_env();
    let pool = sqlx::PgPool::connect(&cfg.database_url)
        .await
        .map_err(|e| format!("cannot connect to DATABASE_URL: {e}"))?;
    admin::set_plan(&pool, email, plan).await
}

async fn serve() {
    let cfg = HubConfig::from_env();
    let pool = sqlx::PgPool::connect(&cfg.database_url)
        .await
        .expect("connect database");
    sqlx::migrate!().run(&pool).await.expect("run migrations");
    let embedder = tokio::task::spawn_blocking(FastEmbedder::new)
        .await
        .expect("embedder task")
        .expect("load embedding model");
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind");
    axum::serve(
        listener,
        app(AppState::new(pool, cfg, std::sync::Arc::new(embedder))),
    )
    .await
    .expect("serve");
}
