//! `ah update`: replace the running binary with the latest (or a given) GitHub release.

use serde::Deserialize;
use std::{io::Write, path::Path, time::Duration};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub const DEFAULT_API_BASE: &str = "https://api.github.com";
const REPO: &str = "c4nnh/agent-hub";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Release target for an OS/arch pair (`std::env::consts` values).
pub fn target_for(os: &str, arch: &str) -> Result<&'static str, Error> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err(format!("no prebuilt binary for {os}/{arch}; build from source instead").into()),
    }
}

pub fn target_triple() -> Result<&'static str, Error> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Updates `dest` in place. `api_base` is the GitHub API root; `version` is an explicit tag.
pub async fn run_update(
    api_base: &str,
    version: Option<&str>,
    dest: &Path,
    out: &mut impl Write,
) -> Result<(), Error> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("ah/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let base = api_base.trim_end_matches('/');
    let url = match version {
        Some(tag) => format!("{base}/repos/{REPO}/releases/tags/{tag}"),
        None => format!("{base}/repos/{REPO}/releases/latest"),
    };
    let res = http.get(&url).send().await?;
    if !res.status().is_success() {
        return Err(format!("could not fetch release info from {url}: {}", res.status()).into());
    }
    let release: Release = res.json().await?;
    let new_version = release.tag_name.trim_start_matches('v');

    if version.is_none() && new_version == CURRENT {
        writeln!(out, "already up to date ({CURRENT})")?;
        return Ok(());
    }

    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let asset_name = format!("ah-{}.{ext}", target_triple()?);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| format!("release {} has no asset {asset_name}", release.tag_name))?;

    tracing::info!(tag = %release.tag_name, asset = %asset.name, "downloading release asset");
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("asset");
    // Binaries are a few MB but links can be slow, so this call gets a longer timeout.
    let res = http
        .get(&asset.browser_download_url)
        .timeout(Duration::from_secs(300))
        .send()
        .await?;
    if !res.status().is_success() {
        return Err(format!("download of {asset_name} failed: {}", res.status()).into());
    }
    std::fs::write(&archive, res.bytes().await?)?;

    let extract_dir = tmp.path().join("out");
    std::fs::create_dir(&extract_dir)?;
    let status = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&extract_dir)
        .status();
    let is_extracted = status.map(|s| s.success()).unwrap_or(false);
    if !is_extracted {
        return Err(format!(
            "could not extract {asset_name} with `tar`; install tar or download it manually from https://github.com/{REPO}/releases"
        )
        .into());
    }
    let bin_name = if cfg!(windows) { "ah.exe" } else { "ah" };
    let extracted = extract_dir.join(bin_name);
    if !extracted.is_file() {
        return Err(format!("{asset_name} does not contain {bin_name}").into());
    }

    // Stage beside the destination: rename cannot cross filesystems.
    let staged = dest.with_file_name(format!("{bin_name}.new"));
    std::fs::copy(&extracted, &staged)?;
    install_binary(&staged, dest)?;
    writeln!(out, "updated {CURRENT} -> {new_version}")?;
    Ok(())
}

/// Moves `new_binary` over `dest`, which may be the running executable.
pub fn install_binary(new_binary: &Path, dest: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new_binary, std::fs::Permissions::from_mode(0o755))?;
        // Remove first: overwriting a signed binary in place breaks the macOS code-signature
        // cache and the kernel SIGKILLs the next launch. A fresh inode avoids that.
        match std::fs::remove_file(dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        std::fs::rename(new_binary, dest)?;
    }
    #[cfg(windows)]
    {
        // A running exe cannot be deleted or overwritten but can be renamed.
        let old = dest.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        if dest.exists() {
            std::fs::rename(dest, &old)?;
        }
        std::fs::rename(new_binary, dest)?;
    }
    Ok(())
}
