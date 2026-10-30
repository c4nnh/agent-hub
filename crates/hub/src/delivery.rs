//! Per-agent delivery state. Clients report the cursor they advance when messages are
//! delivered or buffered for an agent; the hub never infers delivery from WS pushes.

use crate::{
    auth::CurrentUser,
    channels::{internal, require_member},
    state::AppState,
};
use ah_common::{AgentCursorUpdate, AgentDelivery, DeliveryState};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

/// Same cap as agent names in messages and WS hellos.
const MAX_NAME_CHARS: usize = 128;

pub async fn report(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<AgentCursorUpdate>,
) -> Result<StatusCode, StatusCode> {
    require_member(&s.pool, u.id, channel_id).await?;
    let name_len = body.agent_name.chars().count();
    if !(1..=MAX_NAME_CHARS).contains(&name_len) || body.last_read_id < 0 {
        tracing::warn!(user_id = %u.id, %channel_id, name_len, last_read_id = body.last_read_id, "invalid agent cursor");
        return Err(StatusCode::BAD_REQUEST);
    }
    sqlx::query(
        "insert into agent_cursors (channel_id, agent_name, last_read_id) values ($1, $2, $3)
         on conflict (channel_id, agent_name) do update
         set last_read_id = greatest(agent_cursors.last_read_id, excluded.last_read_id),
             updated_at = now()",
    )
    .bind(channel_id)
    .bind(&body.agent_name)
    .bind(body.last_read_id)
    .execute(&s.pool)
    .await
    .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn state(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<DeliveryState>, StatusCode> {
    require_member(&s.pool, u.id, channel_id).await?;
    let last_message_id: i64 =
        sqlx::query_scalar("select coalesce(max(id), 0) from messages where channel_id = $1")
            .bind(channel_id)
            .fetch_one(&s.pool)
            .await
            .map_err(internal)?;
    let rows: Vec<(String, i64, DateTime<Utc>)> = sqlx::query_as(
        "select agent_name, last_read_id, updated_at from agent_cursors where channel_id = $1",
    )
    .bind(channel_id)
    .fetch_all(&s.pool)
    .await
    .map_err(internal)?;
    let online: HashSet<String> = s
        .registry
        .agents(channel_id)
        .into_iter()
        .map(|a| a.name)
        .collect();
    let mut merged: BTreeMap<String, AgentDelivery> = rows
        .into_iter()
        .map(|(agent_name, last_read_id, updated_at)| {
            let is_online = online.contains(&agent_name);
            (
                agent_name.clone(),
                AgentDelivery {
                    agent_name,
                    last_read_id,
                    updated_at: Some(updated_at),
                    online: is_online,
                },
            )
        })
        .collect();
    for name in online {
        merged.entry(name.clone()).or_insert(AgentDelivery {
            agent_name: name,
            last_read_id: 0,
            updated_at: None,
            online: true,
        });
    }
    Ok(Json(DeliveryState {
        last_message_id,
        agents: merged.into_values().collect(),
    }))
}
