use crate::state::AppState;
use ah_common::{AuthPollResponse, MeResponse};
use axum::{
    Json,
    extract::{FromRequestParts, Query, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{IntoResponse, Redirect, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub plan: String,
}

fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// Creates a token for the user and stores only its sha256 hex. Returns the raw token.
pub async fn issue_token(pool: &PgPool, user_id: Uuid) -> Result<String, sqlx::Error> {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    sqlx::query("insert into tokens (token_hash, user_id) values ($1, $2)")
        .bind(hash_token(&token))
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(token)
}

pub async fn authenticate(pool: &PgPool, bearer: &str) -> Option<User> {
    let found = sqlx::query_as::<_, User>(
        "select u.id, u.email, u.name, u.plan from tokens t join users u on u.id = t.user_id where t.token_hash = $1",
    )
    .bind(hash_token(bearer))
    .fetch_optional(pool)
    .await;
    match found {
        Ok(user) => user,
        Err(error) => {
            tracing::error!(%error, "token lookup failed");
            None
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

pub struct CurrentUser(pub User);

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, StatusCode> {
        let Some(token) = bearer(&parts.headers) else {
            tracing::warn!("missing bearer token");
            return Err(StatusCode::UNAUTHORIZED);
        };
        let user = authenticate(&state.pool, token).await;
        match user {
            Some(user) => Ok(CurrentUser(user)),
            None => {
                tracing::warn!("unknown bearer token");
                Err(StatusCode::UNAUTHORIZED)
            }
        }
    }
}

fn internal(error: impl std::fmt::Display) -> StatusCode {
    tracing::error!(error = %error, "auth request failed");
    StatusCode::INTERNAL_SERVER_ERROR
}

#[derive(Deserialize)]
pub struct StartBody {
    code: String,
}

pub async fn cli_start(
    State(s): State<AppState>,
    Json(body): Json<StartBody>,
) -> Result<StatusCode, StatusCode> {
    if body.code.is_empty() || body.code.len() > 128 {
        tracing::warn!(len = body.code.len(), "invalid cli auth code");
        return Err(StatusCode::BAD_REQUEST);
    }
    sqlx::query("insert into auth_requests (code) values ($1) on conflict do nothing")
        .bind(&body.code)
        .execute(&s.pool)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct CodeQuery {
    code: String,
}

pub async fn google_start(
    State(s): State<AppState>,
    Query(q): Query<CodeQuery>,
) -> Result<Redirect, StatusCode> {
    let redirect_uri = format!("{}/auth/callback", s.cfg.public_url);
    let mut url = reqwest::Url::parse(&s.cfg.google_auth_url()).map_err(internal)?;
    url.query_pairs_mut()
        .append_pair("client_id", &s.cfg.google_client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile")
        .append_pair("state", &q.code);
    Ok(Redirect::temporary(url.as_str()))
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: String,
    state: String,
}

#[derive(Deserialize)]
struct GoogleToken {
    access_token: String,
}

#[derive(Deserialize)]
struct GoogleUser {
    email: String,
    #[serde(default)]
    name: String,
}

async fn google_user(s: &AppState, code: &str) -> Result<GoogleUser, reqwest::Error> {
    let http = reqwest::Client::new();
    let redirect_uri = format!("{}/auth/callback", s.cfg.public_url);
    let token: GoogleToken = http
        .post(s.cfg.google_token_url())
        .form(&[
            ("code", code),
            ("client_id", &s.cfg.google_client_id),
            ("client_secret", &s.cfg.google_client_secret),
            ("redirect_uri", &redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    http.get(s.cfg.google_userinfo_url())
        .bearer_auth(token.access_token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

pub async fn google_callback(
    State(s): State<AppState>,
    Query(q): Query<CallbackQuery>,
) -> Result<&'static str, StatusCode> {
    let pending: Option<bool> =
        sqlx::query_scalar("select token is null from auth_requests where code = $1")
            .bind(&q.state)
            .fetch_optional(&s.pool)
            .await
            .map_err(internal)?;
    if pending != Some(true) {
        tracing::warn!(state = %q.state, "auth request not found or already completed");
        return Err(StatusCode::BAD_REQUEST);
    }
    let gu = google_user(&s, &q.code).await.map_err(|error| {
        tracing::error!(%error, "google exchange failed");
        StatusCode::BAD_GATEWAY
    })?;
    let user_id: Uuid = sqlx::query_scalar(
        "insert into users (id, email, name) values ($1, $2, $3)
         on conflict (email) do update set name = excluded.name returning id",
    )
    .bind(Uuid::new_v4())
    .bind(&gu.email)
    .bind(&gu.name)
    .fetch_one(&s.pool)
    .await
    .map_err(internal)?;
    let token = issue_token(&s.pool, user_id).await.map_err(internal)?;
    sqlx::query("update auth_requests set token = $1 where code = $2")
        .bind(&token)
        .bind(&q.state)
        .execute(&s.pool)
        .await
        .map_err(internal)?;
    Ok("You can close this tab.")
}

/// Returns the token once, then deletes the request so the raw token is not kept.
pub async fn cli_poll(
    State(s): State<AppState>,
    Query(q): Query<CodeQuery>,
) -> Result<Response, StatusCode> {
    let token: Option<String> = sqlx::query_scalar(
        "delete from auth_requests where code = $1 and token is not null returning token",
    )
    .bind(&q.code)
    .fetch_optional(&s.pool)
    .await
    .map_err(internal)?;
    let Some(token) = token else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let user = authenticate(&s.pool, &token)
        .await
        .ok_or_else(|| internal("issued token has no user"))?;
    Ok(Json(AuthPollResponse {
        token,
        email: user.email,
    })
    .into_response())
}

pub async fn logout(
    State(s): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
) -> Result<StatusCode, StatusCode> {
    let token = bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    sqlx::query("delete from tokens where token_hash = $1")
        .bind(hash_token(token))
        .execute(&s.pool)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn me(CurrentUser(u): CurrentUser) -> Json<MeResponse> {
    Json(MeResponse {
        email: u.email,
        name: u.name,
        plan: u.plan,
    })
}
