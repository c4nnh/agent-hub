use crate::{
    bindings::Bindings,
    client::{ClientError, HubClient},
    cursors::Cursors,
};
use ah_common::ChannelSummary;
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
    out: &mut impl Write,
) -> Res {
    let ch = pick_channel(client, channel).await?;
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
        "note: joining only binds the folder; an agent receives messages while it listens (background `ah wait`, or the wait_for_message loop)"
    )?;
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
