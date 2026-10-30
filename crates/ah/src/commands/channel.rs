use crate::{
    bindings::Bindings,
    client::{ClientError, HubClient},
    commands::daemon::harness_template,
    cursors::Cursors,
};
use ah_common::{ChannelSummary, DeliveryState};
use dialoguer::{Confirm, Select};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

type Res = Result<(), Box<dyn std::error::Error>>;

pub struct ConfigArgs {
    pub folder: Option<PathBuf>,
    pub add_extra_folder: Option<PathBuf>,
    pub remove_extra_folder: Option<PathBuf>,
}

/// Exact-name match against the caller's channels.
pub async fn resolve(
    client: &HubClient,
    name: &str,
) -> Result<ChannelSummary, Box<dyn std::error::Error>> {
    let channels = client.list_channels().await?;
    let mut hits = channels.into_iter().filter(|c| c.name == name);
    match (hits.next(), hits.next()) {
        (Some(c), None) => Ok(c),
        (None, _) => Err(format!("no channel named '{name}' among your channels").into()),
        _ => Err(format!("channel name '{name}' is ambiguous").into()),
    }
}

/// Named channel, or an interactive pick when `name` is absent.
async fn pick_channel(
    client: &HubClient,
    name: Option<&str>,
) -> Result<ChannelSummary, Box<dyn std::error::Error>> {
    if let Some(n) = name {
        return resolve(client, n).await;
    }
    let mut channels = client.list_channels().await?;
    if channels.is_empty() {
        return Err("you are not in any channel".into());
    }
    let labels: Vec<_> = channels
        .iter()
        .map(|c| format!("{} (owner {})", c.name, c.owner_email))
        .collect();
    let i = Select::new()
        .with_prompt("Channel")
        .items(&labels)
        .interact()?;
    Ok(channels.swap_remove(i))
}

pub async fn run_create(client: &HubClient, name: &str, out: &mut impl Write) -> Res {
    let c = client.create_channel(name).await?;
    writeln!(out, "Created channel {} ({})", c.name, c.id)?;
    Ok(())
}

pub async fn run_join(
    dir: &Path,
    client: &HubClient,
    cwd: &Path,
    channel: Option<&str>,
    hooks: &crate::hooks::HookDirs,
    harness: super::guide::Harness,
    out: &mut impl Write,
) -> Res {
    let ch = pick_channel(client, channel).await?;
    // Installed before binding, so a bound folder is never without its Stop hook.
    crate::hooks::ensure_and_report(hooks, out)?;
    let mut bindings = Bindings::load(dir)?;
    let folder = Bindings::key(cwd)?;
    let b = bindings.0.entry(folder.clone()).or_default();
    if !b.channel_ids.contains(&ch.id) {
        b.channel_ids.push(ch.id);
    }
    bindings.save(dir)?;
    // Everything posted after this point counts as new for the folder, whenever it starts
    // listening. An existing cursor is delivery history and stays.
    let seed = std::collections::BTreeMap::from([(ch.id, ch.last_message_id)]);
    Cursors::seed(dir, &folder, &seed)?;
    writeln!(out, "Bound {} to channel {}", cwd.display(), ch.name)?;
    writeln!(
        out,
        "note: joining only binds the folder; an agent receives messages while it listens (the wait_for_message loop, a background `ah wait` on Claude Code, or `ah channel wake` with `ah daemon`)"
    )?;
    let has_wake = bindings
        .0
        .get(&folder)
        .is_some_and(|b| b.wake_command.is_some());
    if !has_wake {
        writeln!(
            out,
            "while no session is open and no wake command is set, this agent is unreachable; messages wait until the next turn or listener. (ah channel wake --harness <h> or --notify, plus ah daemon, wakes or alerts when messages arrive)"
        )?;
    }
    let agents = client.list_agents(ch.id).await?;
    if agents.is_empty() {
        writeln!(out, "no active agents")?;
    }
    for a in agents {
        writeln!(
            out,
            "{}  {}  {}",
            a.name,
            a.folder.as_deref().unwrap_or("-"),
            a.connected_at.to_rfc3339()
        )?;
    }
    writeln!(out)?;
    super::guide::run_guide(harness, out)?;
    Ok(())
}

/// Unbinds one folder from one channel. Local only: membership on the hub is untouched; the
/// hub is only read to map channel names to ids.
pub async fn run_leave(
    dir: &Path,
    client: &HubClient,
    folder: &Path,
    channel: Option<&str>,
    out: &mut impl Write,
) -> Res {
    let key = Bindings::key(folder)?;
    let mut bindings = Bindings::load(dir)?;
    let bound_ids = bindings
        .0
        .get(&key)
        .map(|b| b.channel_ids.clone())
        .unwrap_or_default();
    if bound_ids.is_empty() {
        return Err(format!("{key} is not bound to any channel").into());
    }
    let mut bound: Vec<_> = client
        .list_channels()
        .await?
        .into_iter()
        .filter(|c| bound_ids.contains(&c.id))
        .collect();
    let ch = match channel {
        Some(name) => {
            let hit = bound.iter().position(|c| c.name == name);
            let i = hit.ok_or_else(|| format!("{key} is not bound to channel {name}"))?;
            bound.swap_remove(i)
        }
        None if bound.len() == 1 => bound.swap_remove(0),
        None if bound.is_empty() => {
            return Err(format!("none of the channels bound to {key} are visible to you").into());
        }
        None => {
            let labels: Vec<_> = bound.iter().map(|c| c.name.clone()).collect();
            let i = Select::new()
                .with_prompt("Channel")
                .items(&labels)
                .interact()?;
            bound.swap_remove(i)
        }
    };
    if let Some(b) = bindings.0.get_mut(&key) {
        b.channel_ids.retain(|id| *id != ch.id);
        if b.channel_ids.is_empty() {
            bindings.0.remove(&key);
        }
    }
    bindings.save(dir)?;
    Cursors::remove(dir, &key, ch.id)?;
    writeln!(out, "unbound {key} from channel {}", ch.name)?;
    Ok(())
}

/// Leaves the channel on the hub and drops it from every folder binding.
pub async fn run_leave_account(
    dir: &Path,
    client: &HubClient,
    channel: Option<&str>,
    yes: bool,
    out: &mut impl Write,
) -> Res {
    let ch = pick_channel(client, channel).await?;
    if !yes {
        let ok = Confirm::new()
            .with_prompt(format!("Leave channel {}?", ch.name))
            .interact()?;
        if !ok {
            writeln!(out, "Cancelled")?;
            return Ok(());
        }
    }
    let left = client.leave_channel(ch.id).await;
    match left {
        Ok(()) => {}
        // Owner with members: still in the channel, keep bindings.
        Err(ClientError::Status {
            status: 409,
            message,
        }) => {
            return Err(format!("cannot leave: {message}").into());
        }
        // Channel is gone or access revoked: bindings must not point at it.
        Err(ClientError::Status { status, message }) if (400..500).contains(&status) => {
            writeln!(out, "warning: hub returned {status}: {message}")?;
        }
        Err(e) => return Err(e.into()),
    }
    let mut bindings = Bindings::load(dir)?;
    for b in bindings.0.values_mut() {
        b.channel_ids.retain(|id| *id != ch.id);
    }
    bindings.save(dir)?;
    writeln!(out, "Left channel {}", ch.name)?;
    Ok(())
}

pub async fn run_invite(
    client: &HubClient,
    channel: &str,
    email: &str,
    out: &mut impl Write,
) -> Res {
    let ch = resolve(client, channel).await?;
    client.invite(ch.id, email).await?;
    writeln!(out, "Invited {email} to {}", ch.name)?;
    Ok(())
}

/// `respond`: `Some((id, accept))` answers directly. Otherwise prints the pending list, then
/// prompts to answer one only when `is_tty` (callers pass `stdin().is_terminal()`).
pub async fn run_invitation(
    client: &HubClient,
    respond: Option<(Uuid, bool)>,
    is_tty: bool,
    out: &mut impl Write,
) -> Res {
    if let Some((id, accept)) = respond {
        client.respond_invitation(id, accept).await?;
        writeln!(
            out,
            "{} invitation {id}",
            if accept { "Accepted" } else { "Rejected" }
        )?;
        return Ok(());
    }
    let invs = client.list_invitations().await?;
    if invs.is_empty() {
        writeln!(out, "no pending invitations")?;
        return Ok(());
    }
    for i in &invs {
        writeln!(out, "{}  {}  {}", i.id, i.channel_name, i.invited_by_email)?;
    }
    if !is_tty {
        return Ok(());
    }
    let labels: Vec<_> = invs
        .iter()
        .map(|i| format!("{} (from {})", i.channel_name, i.invited_by_email))
        .collect();
    let i = Select::new()
        .with_prompt("Invitation")
        .items(&labels)
        .interact()?;
    let a = Select::new()
        .with_prompt("Respond")
        .items(&["Accept", "Reject"])
        .interact()?;
    let accept = a == 0;
    client.respond_invitation(invs[i].id, accept).await?;
    writeln!(
        out,
        "{} invitation {}",
        if accept { "Accepted" } else { "Rejected" },
        invs[i].id
    )?;
    Ok(())
}

pub fn run_config(dir: &Path, cwd: &Path, args: ConfigArgs, out: &mut impl Write) -> Res {
    let mut bindings = Bindings::load(dir)?;
    let mut key = Bindings::key(cwd)?;
    if let Some(target) = &args.folder {
        let new_key = Bindings::key(target)?;
        if new_key != key {
            if bindings.0.contains_key(&new_key) {
                return Err(format!("{new_key} is already bound; refusing to overwrite").into());
            }
            let moved = bindings
                .0
                .remove(&key)
                .ok_or("current folder has no binding; run `ah channel join` first")?;
            bindings.0.insert(new_key.clone(), moved);
            key = new_key;
        }
    }
    if let Some(b) = bindings.0.get_mut(&key) {
        if let Some(p) = &args.add_extra_folder {
            let p = Bindings::key(p)?;
            if !b.extra_folders.contains(&p) {
                b.extra_folders.push(p);
            }
        }
        if let Some(p) = &args.remove_extra_folder {
            let p = Bindings::key(p).unwrap_or_else(|_| p.to_string_lossy().into_owned());
            b.extra_folders.retain(|f| *f != p);
        }
    } else if args.add_extra_folder.is_some() || args.remove_extra_folder.is_some() {
        return Err("current folder has no binding; run `ah channel join` first".into());
    }
    bindings.save(dir)?;
    match bindings.0.get(&key) {
        None => writeln!(out, "{key} has no binding")?,
        Some(b) => {
            writeln!(out, "folder: {key}")?;
            for id in &b.channel_ids {
                writeln!(out, "channel: {id}")?;
            }
            for f in &b.extra_folders {
                writeln!(out, "extra folder: {f}")?;
            }
        }
    }
    Ok(())
}

pub struct WakeArgs {
    pub folder: Option<PathBuf>,
    pub harness: Option<String>,
    pub command: Option<String>,
    pub off: bool,
    /// Same as `command: Some("notify")`.
    pub notify: bool,
}

/// Sets, clears or prints the wake command of a bound folder (`--folder`, else `cwd`).
pub fn run_wake(dir: &Path, cwd: &Path, args: WakeArgs, out: &mut impl Write) -> Res {
    let key = Bindings::key(args.folder.as_deref().unwrap_or(cwd))?;
    let mut bindings = Bindings::load(dir)?;
    let b = bindings
        .0
        .get_mut(&key)
        .ok_or_else(|| format!("{key} is not bound to a channel; run `ah channel join` first"))?;
    if args.command.as_deref().is_some_and(|c| c.trim().is_empty()) {
        return Err("--command must not be empty".into());
    }
    let command = if args.notify {
        Some(super::daemon::NOTIFY.to_string())
    } else {
        args.command.clone()
    };
    if cfg!(not(unix)) && command.as_deref().is_some_and(super::daemon::is_notify) {
        return Err("notify wake is not supported on this platform yet".into());
    }
    let new = match (&args.harness, &command, args.off) {
        (Some(h), None, false) => {
            let t = harness_template(h).ok_or_else(|| format!("unknown harness '{h}'"))?;
            Some(Some(t.to_string()))
        }
        (None, Some(c), false) => Some(Some(c.clone())),
        (None, None, true) => Some(None),
        (None, None, false) => None,
        _ => return Err("use only one of --harness, --command, --off".into()),
    };
    let Some(new) = new else {
        match &b.wake_command {
            Some(c) => writeln!(out, "{c}")?,
            None => writeln!(out, "none")?,
        }
        return Ok(());
    };
    b.wake_command = new;
    let msg = match &b.wake_command {
        Some(c) => format!("wake command for {key}: {c}"),
        None => format!("wake command removed for {key}"),
    };
    bindings.save(dir)?;
    writeln!(out, "{msg}")?;
    Ok(())
}

/// Delivery lines for one channel: a header, then one line per agent that reported or is online.
/// An agent whose reported cursor trails the channel is marked behind.
pub fn format_delivery(channel: &str, state: &DeliveryState) -> String {
    let mut text = format!("#{channel} latest message id {}", state.last_message_id);
    for a in &state.agents {
        let presence = if a.online { "online" } else { "offline" };
        let behind = if a.last_read_id < state.last_message_id {
            ", behind"
        } else {
            ""
        };
        let coverage = match (a.online, a.covered) {
            (true, _) => "",
            (false, true) => ", covered",
            (false, false) => ", unreachable while idle",
        };
        text.push_str(&format!(
            "\n{}: read up to {} of {}, {presence}{behind}{coverage}",
            a.agent_name, a.last_read_id, state.last_message_id
        ));
    }
    text
}

/// `ah channel status`: delivery state of one channel, or of every channel bound to the folder.
pub async fn run_status(
    dir: &Path,
    client: &HubClient,
    folder: &Path,
    channel: Option<&str>,
    out: &mut impl Write,
) -> Res {
    let bindings = Bindings::load(dir)?;
    let bound = bindings.for_folder(folder).cloned().unwrap_or_default();
    let all = client.list_channels().await?;
    let mut channels: Vec<ChannelSummary> = all
        .into_iter()
        .filter(|c| bound.channel_ids.contains(&c.id))
        .collect();
    if let Some(name) = channel {
        channels.retain(|c| c.name == name);
        if channels.is_empty() {
            return Err(format!("channel {name} is not bound to this folder").into());
        }
    }
    if channels.is_empty() {
        return Err(crate::mcp::UNBOUND.into());
    }
    for ch in channels {
        let state = client.delivery(ch.id).await?;
        writeln!(out, "{}", format_delivery(&ch.name, &state))?;
    }
    Ok(())
}
