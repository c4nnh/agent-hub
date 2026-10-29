use crate::{auth::CurrentUser, channels::require_member, state::AppState, ws::Registry};
use ah_common::{Message, PostMessage, SenderType};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

const MAX_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

fn internal(error: impl std::fmt::Display) -> StatusCode {
    tracing::error!(error = %error, "message request failed");
    StatusCode::INTERNAL_SERVER_ERROR
}

type Row = (
    i64,
    Uuid,
    String,
    String,
    String,
    Option<i64>,
    Vec<String>,
    DateTime<Utc>,
);

const COLUMNS: &str =
    "id, channel_id, sender_type, sender_name, body, reply_to, mentions, created_at";

fn to_message(row: Row) -> Message {
    let (id, channel_id, sender_type, sender_name, body, reply_to, mentions, created_at) = row;
    let sender_type = if sender_type == "agent" {
        SenderType::Agent
    } else {
        SenderType::Human
    };
    Message {
        id,
        channel_id,
        sender_type,
        sender_name,
        body,
        reply_to,
        mentions,
        created_at,
    }
}

pub async fn post(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<PostMessage>,
) -> Result<(StatusCode, Json<Message>), StatusCode> {
    require_member(&s.pool, u.id, channel_id).await?;
    let name_len = req.sender_name.chars().count();
    if req.body.is_empty() || req.body.len() > MAX_BODY_BYTES || !(1..=128).contains(&name_len) {
        tracing::warn!(
            user_id = %u.id,
            %channel_id,
            body_len = req.body.len(),
            name_len,
            "invalid message"
        );
        return Err(StatusCode::BAD_REQUEST);
    }
    // axum drops this future when the HTTP client disconnects. A commit without its publish
    // would be skipped forever by client cursors, so commit and publish run in their own task,
    // which finishes even if nobody waits for it.
    let task = tokio::spawn(insert_and_publish(
        s.pool.clone(),
        s.registry.clone(),
        u.id,
        channel_id,
        req,
    ));
    let message = task.await.map_err(internal)??;
    Ok((StatusCode::CREATED, Json(message)))
}

async fn insert_and_publish(
    pool: PgPool,
    registry: Registry,
    user_id: Uuid,
    channel_id: Uuid,
    req: PostMessage,
) -> Result<Message, StatusCode> {
    // Clients track a per-channel high-water id and drop anything at or below it, so ids must
    // commit and publish in order per channel. The advisory lock orders id assignment and commit
    // across hub processes; the in-process lock also covers the gap between commit and publish.
    // ponytail: one lock for all channels per process; make it per channel if posts contend.
    let _order = registry.post_order.clone().lock_owned().await;
    let mut tx = pool.begin().await.map_err(internal)?;
    sqlx::query("select pg_advisory_xact_lock(hashtext($1::uuid::text))")
        .bind(channel_id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    let row: Row = sqlx::query_as(&format!(
        "insert into messages (channel_id, sender_user_id, sender_type, sender_name, body, reply_to, mentions)
         values ($1, $2, $3, $4, $5, $6, $7) returning {COLUMNS}"
    ))
    .bind(channel_id)
    .bind(user_id)
    .bind(req.sender_type.as_str())
    .bind(&req.sender_name)
    .bind(&req.body)
    .bind(req.reply_to)
    .bind(&req.mentions)
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    let message = to_message(row);
    registry.publish(channel_id, &message);
    Ok(message)
}

#[derive(Deserialize)]
pub struct ListQuery {
    since_id: Option<i64>,
    limit: Option<i64>,
}

pub async fn list(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Message>>, StatusCode> {
    require_member(&s.pool, u.id, channel_id).await?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "select {COLUMNS} from messages where channel_id = $1 and id > $2 order by id limit $3"
    ))
    .bind(channel_id)
    .bind(q.since_id.unwrap_or(0))
    .bind(limit)
    .fetch_all(&s.pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows.into_iter().map(to_message).collect()))
}
