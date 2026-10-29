use crate::{auth::CurrentUser, state::AppState};
use ah_common::{
    ChannelSummary, CreateChannel, InvitationSummary, InviteRequest, RespondInvitation,
};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Member,
}

fn internal(error: impl std::fmt::Display) -> StatusCode {
    tracing::error!(error = %error, "channel request failed");
    StatusCode::INTERNAL_SERVER_ERROR
}

fn parse_role(role: &str) -> Role {
    if role == "owner" {
        Role::Owner
    } else {
        Role::Member
    }
}

/// Returns the caller's role in the channel, or 403 when they are not a member.
pub async fn require_member(
    pool: &PgPool,
    user_id: Uuid,
    channel_id: Uuid,
) -> Result<Role, StatusCode> {
    let role: Option<String> = sqlx::query_scalar(
        "select role from channel_members where channel_id = $1 and user_id = $2",
    )
    .bind(channel_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map_err(internal)?;
    match role {
        Some(role) => Ok(parse_role(&role)),
        None => {
            tracing::warn!(%user_id, %channel_id, "not a channel member");
            Err(StatusCode::FORBIDDEN)
        }
    }
}

pub async fn list(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
) -> Result<Json<Vec<ChannelSummary>>, StatusCode> {
    let rows: Vec<(Uuid, String, String, String, i64)> = sqlx::query_as(
        "select c.id, c.name, m.role, o.email,
                coalesce((select max(x.id) from messages x where x.channel_id = c.id), 0)
         from channel_members m
         join channels c on c.id = m.channel_id
         join users o on o.id = c.owner_id
         where m.user_id = $1
         order by c.created_at",
    )
    .bind(u.id)
    .fetch_all(&s.pool)
    .await
    .map_err(internal)?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(id, name, role, owner_email, last_message_id)| ChannelSummary {
                    id,
                    name,
                    role,
                    owner_email,
                    last_message_id,
                },
            )
            .collect(),
    ))
}

pub async fn create(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Json(body): Json<CreateChannel>,
) -> Result<(StatusCode, Json<ChannelSummary>), StatusCode> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        tracing::warn!(user_id = %u.id, "empty channel name");
        return Err(StatusCode::BAD_REQUEST);
    }
    let id = Uuid::new_v4();
    let mut tx = s.pool.begin().await.map_err(internal)?;
    let inserted = sqlx::query("insert into channels (id, name, owner_id) values ($1, $2, $3)")
        .bind(id)
        .bind(&name)
        .bind(u.id)
        .execute(&mut *tx)
        .await;
    if let Err(error) = inserted {
        let is_duplicate = error
            .as_database_error()
            .is_some_and(|e| e.is_unique_violation());
        if is_duplicate {
            tracing::warn!(user_id = %u.id, %name, "duplicate channel name");
            return Err(StatusCode::CONFLICT);
        }
        return Err(internal(error));
    }
    sqlx::query("insert into channel_members (channel_id, user_id, role) values ($1, $2, 'owner')")
        .bind(id)
        .bind(u.id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(ChannelSummary {
            id,
            name,
            role: "owner".into(),
            owner_email: u.email,
            last_message_id: 0,
        }),
    ))
}

pub async fn leave(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    let mut tx = s.pool.begin().await.map_err(internal)?;
    // Lock the channel row so concurrent leaves and accepts serialize.
    let locked: Option<Uuid> =
        sqlx::query_scalar("select id from channels where id = $1 for update")
            .bind(channel_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;
    if locked.is_none() {
        tracing::warn!(user_id = %u.id, %channel_id, "leave on missing channel");
        return Err(StatusCode::FORBIDDEN);
    }
    let role: Option<String> = sqlx::query_scalar(
        "select role from channel_members where channel_id = $1 and user_id = $2",
    )
    .bind(channel_id)
    .bind(u.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?;
    let Some(role) = role else {
        tracing::warn!(user_id = %u.id, %channel_id, "leave by non-member");
        return Err(StatusCode::FORBIDDEN);
    };
    if parse_role(&role) == Role::Owner {
        let others: i64 = sqlx::query_scalar(
            "select count(*) from channel_members where channel_id = $1 and user_id <> $2",
        )
        .bind(channel_id)
        .bind(u.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        if others > 0 {
            tracing::warn!(user_id = %u.id, %channel_id, others, "owner cannot leave with members");
            return Err(StatusCode::CONFLICT);
        }
        // The owner is the last member: remove the channel and everything under it.
        for table in [
            "messages",
            "context_entries",
            "invitations",
            "channel_members",
        ] {
            sqlx::query(&format!("delete from {table} where channel_id = $1"))
                .bind(channel_id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        sqlx::query("delete from channels where id = $1")
            .bind(channel_id)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
    } else {
        sqlx::query("delete from channel_members where channel_id = $1 and user_id = $2")
            .bind(channel_id)
            .bind(u.id)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn invite(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<InviteRequest>,
) -> Result<StatusCode, StatusCode> {
    require_member(&s.pool, u.id, channel_id).await?;
    let email = body.email.trim().to_lowercase();
    if !email.contains('@') {
        tracing::warn!(user_id = %u.id, "invalid invite email");
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut tx = s.pool.begin().await.map_err(internal)?;
    // Serialize invites per channel so the duplicate checks below are not racy.
    sqlx::query("select id from channels where id = $1 for update")
        .bind(channel_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;
    let is_member: bool = sqlx::query_scalar(
        "select exists (select 1 from channel_members m join users x on x.id = m.user_id
         where m.channel_id = $1 and lower(x.email) = $2)",
    )
    .bind(channel_id)
    .bind(&email)
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    let is_pending: bool = sqlx::query_scalar(
        "select exists (select 1 from invitations
         where channel_id = $1 and lower(email) = $2 and status = 'pending')",
    )
    .bind(channel_id)
    .bind(&email)
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    if is_member || is_pending {
        tracing::warn!(%channel_id, %email, is_member, is_pending, "invite conflict");
        return Err(StatusCode::CONFLICT);
    }
    sqlx::query(
        "insert into invitations (id, channel_id, email, invited_by) values ($1, $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(channel_id)
    .bind(&email)
    .bind(u.id)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_invitations(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
) -> Result<Json<Vec<InvitationSummary>>, StatusCode> {
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "select i.id, c.name, b.email
         from invitations i
         join channels c on c.id = i.channel_id
         join users b on b.id = i.invited_by
         where lower(i.email) = lower($1) and i.status = 'pending'
         order by i.created_at",
    )
    .bind(&u.email)
    .fetch_all(&s.pool)
    .await
    .map_err(internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, channel_name, invited_by_email)| InvitationSummary {
                id,
                channel_name,
                invited_by_email,
            })
            .collect(),
    ))
}

pub async fn respond(
    State(s): State<AppState>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<Uuid>,
    Json(body): Json<RespondInvitation>,
) -> Result<StatusCode, StatusCode> {
    let mut tx = s.pool.begin().await.map_err(internal)?;
    let found: Option<(Uuid, String)> = sqlx::query_as(
        "select channel_id, status from invitations
         where id = $1 and lower(email) = lower($2) for update",
    )
    .bind(id)
    .bind(&u.email)
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?;
    let Some((channel_id, status)) = found else {
        tracing::warn!(user_id = %u.id, invitation_id = %id, "invitation not found for caller");
        return Err(StatusCode::NOT_FOUND);
    };
    if status != "pending" {
        tracing::warn!(user_id = %u.id, invitation_id = %id, %status, "invitation already answered");
        return Err(StatusCode::CONFLICT);
    }
    let new_status = if body.accept { "accepted" } else { "rejected" };
    sqlx::query("update invitations set status = $1 where id = $2")
        .bind(new_status)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    if body.accept {
        sqlx::query(
            "insert into channel_members (channel_id, user_id, role) values ($1, $2, 'member')
             on conflict do nothing",
        )
        .bind(channel_id)
        .bind(u.id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}
