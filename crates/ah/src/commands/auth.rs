use crate::{
    client::HubClient,
    config::{CliConfig, Credentials},
};
use std::{io::Write, path::Path, time::Duration};

pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
pub const POLL_TIMEOUT: Duration = Duration::from_secs(600);

pub async fn run_auth(
    dir: &Path,
    cfg: &CliConfig,
    no_browser: bool,
    poll_interval: Duration,
    poll_timeout: Duration,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let existing = Credentials::load(dir)?;
    if let Some(c) = existing {
        writeln!(out, "Already logged in as {}", c.email)?;
        return Ok(());
    }
    let client = HubClient::new(cfg.hub_url.clone(), None);
    let code = uuid::Uuid::new_v4().to_string();
    client.auth_start(&code).await?;
    let url = client.url(&format!("/auth/start?code={code}"));
    writeln!(
        out,
        "Verification code: {} — confirm it matches in the browser.",
        ah_common::user_code(&code)
    )?;
    if no_browser || open::that(&url).is_err() {
        writeln!(out, "Open this URL to log in:\n{url}")?;
    } else {
        writeln!(
            out,
            "Opened browser to log in. If it did not open, visit:\n{url}"
        )?;
    }
    out.flush()?;
    let deadline = tokio::time::Instant::now() + poll_timeout;
    loop {
        let polled = client.auth_poll(&code).await?;
        if let Some(r) = polled {
            Credentials {
                token: r.token,
                email: r.email.clone(),
            }
            .save(dir)?;
            writeln!(out, "Logged in as {}", r.email)?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("login timed out; run `ah auth login` again".into());
        }
        tokio::time::sleep(poll_interval).await;
    }
}

pub async fn run_logout(
    dir: &Path,
    cfg: &CliConfig,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let creds = Credentials::load(dir)?;
    let Some(c) = creds else {
        writeln!(out, "Not logged in")?;
        return Ok(());
    };
    let client = HubClient::new(cfg.hub_url.clone(), Some(c.token));
    let revoked = client.logout().await;
    if let Err(e) = revoked {
        writeln!(err, "warning: could not revoke token on hub: {e}")?;
    }
    Credentials::delete(dir)?;
    writeln!(out, "Logged out")?;
    Ok(())
}
