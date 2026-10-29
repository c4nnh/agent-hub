use crate::{auth::CurrentUser, channels::require_member, config::HubMode, state::AppState};
use ah_common::{ContextEntry, ContextEntrySummary, ContextHit, UpsertContext};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_TITLE_CHARS: usize = 200;
const DEFAULT_K: i64 = 5;
const MAX_K: i64 = 50;

type Row = (Uuid, Uuid, String, String, i32, String, DateTime<Utc>);
const COLUMNS: &str = "id, channel_id, title, body, version, updated_by, updated_at";

fn to_entry(r: Row) -> ContextEntry {
    ContextEntry {
        id: r.0,
        channel_id: r.1,
        title: r.2,
        body: r.3,
        version: r.4,
        updated_by: r.5,
        updated_at: r.6,
    }
}

fn internal(error: impl std::fmt::Display) -> Response {
    tracing::error!(error = %error, "context request failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn status(code: StatusCode) -> Response {
    code.into_response()
}

fn check_plan(s: &AppState, user: &crate::auth::User) -> Result<(), Response> {
    if s.cfg.hub_mode == HubMode::Hosted && user.plan != "paid" {
        tracing::warn!(user_id = %user.id, plan = %user.plan, "context store requires paid plan");
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "code": "paid_required",
                "message": "Context store requires a paid plan on the hosted hub"
            })),
        )
            .into_response());
    }
    Ok(())
}

async fn member(s: &AppState, user_id: Uuid, channel_id: Uuid) -> Result<(), Response> {
    require_member(&s.pool, user_id, channel_id)
        .await
        .map(|_| ())
        .map_err(status)
}

async fn embed(s: &AppState, text: String) -> Result<Vector, Response> {
    let embedder = s.embedder.clone();
    let joined = tokio::task::spawn_blocking(move || embedder.embed(&text)).await;
    joined.map(Vector::from).map_err(internal)
}

pub async fn list(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<Vec<ContextEntrySummary>>, Response> {
    check_plan(&s, &u)?;
    member(&s, u.id, channel_id).await?;
    let rows: Vec<(Uuid, String, i32, DateTime<Utc>)> = sqlx::query_as(
        "select id, title, version, updated_at from context_entries
         where channel_id = $1 order by updated_at desc",
    )
    .bind(channel_id)
    .fetch_all(&s.pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows.into_iter().map(summary).collect()))
}

fn summary(r: (Uuid, String, i32, DateTime<Utc>)) -> ContextEntrySummary {
    ContextEntrySummary {
        id: r.0,
        title: r.1,
        version: r.2,
        updated_at: r.3,
    }
}

pub async fn get(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ContextEntry>, Response> {
    check_plan(&s, &u)?;
    let row: Option<Row> = sqlx::query_as(&format!(
        "select {COLUMNS} from context_entries where id = $1"
    ))
    .bind(id)
    .fetch_optional(&s.pool)
    .await
    .map_err(internal)?;
    let Some(row) = row else {
        tracing::warn!(user_id = %u.id, entry_id = %id, "context entry not found");
        return Err(status(StatusCode::NOT_FOUND));
    };
    let entry = to_entry(row);
    member(&s, u.id, entry.channel_id).await?;
    Ok(Json(entry))
}

pub async fn upsert(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<UpsertContext>,
) -> Result<Response, Response> {
    check_plan(&s, &u)?;
    member(&s, u.id, channel_id).await?;
    let title_len = req.title.chars().count();
    let invalid = !(1..=MAX_TITLE_CHARS).contains(&title_len)
        || req.body.is_empty()
        || req.body.len() > MAX_BODY_BYTES
        || (req.id.is_some() && req.base_version.is_none());
    if invalid {
        tracing::warn!(
            user_id = %u.id,
            %channel_id,
            title_len,
            body_len = req.body.len(),
            has_id = req.id.is_some(),
            has_base_version = req.base_version.is_some(),
            "invalid context entry"
        );
        return Err(status(StatusCode::BAD_REQUEST));
    }
    let embedding = embed(&s, format!("{}\n{}", req.title, req.body)).await?;

    let (Some(id), Some(base_version)) = (req.id, req.base_version) else {
        let row: Row = sqlx::query_as(&format!(
            "insert into context_entries (id, channel_id, title, body, updated_by, embedding)
             values ($1, $2, $3, $4, $5, $6) returning {COLUMNS}"
        ))
        .bind(Uuid::new_v4())
        .bind(channel_id)
        .bind(&req.title)
        .bind(&req.body)
        .bind(&u.email)
        .bind(embedding)
        .fetch_one(&s.pool)
        .await
        .map_err(internal)?;
        return Ok((StatusCode::CREATED, Json(to_entry(row))).into_response());
    };

    let updated: Option<Row> = sqlx::query_as(&format!(
        "update context_entries
         set title = $3, body = $4, embedding = $5, updated_by = $6,
             version = version + 1, updated_at = now()
         where id = $1 and version = $2 and channel_id = $7 returning {COLUMNS}"
    ))
    .bind(id)
    .bind(base_version)
    .bind(&req.title)
    .bind(&req.body)
    .bind(embedding)
    .bind(&u.email)
    .bind(channel_id)
    .fetch_optional(&s.pool)
    .await
    .map_err(internal)?;
    if let Some(row) = updated {
        return Ok(Json(to_entry(row)).into_response());
    }

    let current: Option<Row> = sqlx::query_as(&format!(
        "select {COLUMNS} from context_entries where id = $1 and channel_id = $2"
    ))
    .bind(id)
    .bind(channel_id)
    .fetch_optional(&s.pool)
    .await
    .map_err(internal)?;
    match current {
        Some(row) => {
            tracing::warn!(user_id = %u.id, entry_id = %id, base_version, "stale context base_version");
            Ok((StatusCode::CONFLICT, Json(to_entry(row))).into_response())
        }
        None => {
            tracing::warn!(user_id = %u.id, entry_id = %id, %channel_id, "context entry not found in channel");
            Err(status(StatusCode::NOT_FOUND))
        }
    }
}

#[derive(Deserialize)]
pub struct SearchRequest {
    query: String,
    #[serde(default)]
    k: Option<i64>,
}

pub async fn search(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<Vec<ContextHit>>, Response> {
    check_plan(&s, &u)?;
    member(&s, u.id, channel_id).await?;
    if req.query.is_empty() {
        tracing::warn!(user_id = %u.id, %channel_id, "empty context search query");
        return Err(status(StatusCode::BAD_REQUEST));
    }
    let k = req.k.unwrap_or(DEFAULT_K).clamp(1, MAX_K);
    let query_vec = embed(&s, req.query).await?;
    let rows: Vec<(Uuid, String, i32, DateTime<Utc>, f32)> = sqlx::query_as(
        "select id, title, version, updated_at, (1 - (embedding <=> $1))::float4
         from context_entries where channel_id = $2 and embedding is not null
         order by embedding <=> $1 limit $3",
    )
    .bind(query_vec)
    .bind(channel_id)
    .bind(k)
    .fetch_all(&s.pool)
    .await
    .map_err(internal)?;
    let hits = rows
        .into_iter()
        .map(|r| ContextHit {
            entry: summary((r.0, r.1, r.2, r.3)),
            score: r.4,
        })
        .collect();
    Ok(Json(hits))
}
