use ah_common::{AgentPresence, AuthPollResponse, ChannelSummary, InvitationSummary, MeResponse};
use reqwest::{Response, StatusCode};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("hub returned {status}: {message}")]
    Status { status: u16, message: String },
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
}

pub struct HubClient {
    hub_url: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl HubClient {
    pub fn new(hub_url: String, token: Option<String>) -> Self {
        Self {
            hub_url: hub_url.trim_end_matches('/').to_string(),
            token,
            http: reqwest::Client::new(),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.hub_url)
    }

    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let rb = self.http.request(method, self.url(path));
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    /// Maps non-2xx responses to `ClientError::Status`.
    pub async fn check(res: Response) -> Result<Response, ClientError> {
        if res.status().is_success() {
            return Ok(res);
        }
        let status = res.status().as_u16();
        let message = res.text().await.unwrap_or_default();
        Err(ClientError::Status { status, message })
    }

    pub async fn auth_start(&self, code: &str) -> Result<(), ClientError> {
        let res = self
            .request(reqwest::Method::POST, "/api/auth/cli/start")
            .json(&serde_json::json!({ "code": code }))
            .send()
            .await?;
        Self::check(res).await.map(|_| ())
    }

    /// `None` while the login is still pending.
    pub async fn auth_poll(&self, code: &str) -> Result<Option<AuthPollResponse>, ClientError> {
        let res = self
            .request(reqwest::Method::GET, "/api/auth/cli/poll")
            .query(&[("code", code)])
            .send()
            .await?;
        let res = Self::check(res).await?;
        if res.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        Ok(Some(res.json().await?))
    }

    pub async fn logout(&self) -> Result<(), ClientError> {
        let res = self
            .request(reqwest::Method::POST, "/api/auth/logout")
            .send()
            .await?;
        Self::check(res).await.map(|_| ())
    }

    pub async fn me(&self) -> Result<MeResponse, ClientError> {
        let res = self.request(reqwest::Method::GET, "/api/me").send().await?;
        Ok(Self::check(res).await?.json().await?)
    }
}

impl HubClient {
    pub async fn list_channels(&self) -> Result<Vec<ChannelSummary>, ClientError> {
        let res = self
            .request(reqwest::Method::GET, "/api/channels")
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    pub async fn create_channel(&self, name: &str) -> Result<ChannelSummary, ClientError> {
        let res = self
            .request(reqwest::Method::POST, "/api/channels")
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    pub async fn leave_channel(&self, id: Uuid) -> Result<(), ClientError> {
        let res = self
            .request(reqwest::Method::POST, &format!("/api/channels/{id}/leave"))
            .send()
            .await?;
        Self::check(res).await.map(|_| ())
    }

    pub async fn invite(&self, id: Uuid, email: &str) -> Result<(), ClientError> {
        let res = self
            .request(
                reqwest::Method::POST,
                &format!("/api/channels/{id}/invitations"),
            )
            .json(&serde_json::json!({ "email": email }))
            .send()
            .await?;
        Self::check(res).await.map(|_| ())
    }

    pub async fn list_invitations(&self) -> Result<Vec<InvitationSummary>, ClientError> {
        let res = self
            .request(reqwest::Method::GET, "/api/invitations")
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    pub async fn respond_invitation(&self, id: Uuid, accept: bool) -> Result<(), ClientError> {
        let res = self
            .request(
                reqwest::Method::POST,
                &format!("/api/invitations/{id}/respond"),
            )
            .json(&serde_json::json!({ "accept": accept }))
            .send()
            .await?;
        Self::check(res).await.map(|_| ())
    }

    pub async fn list_agents(&self, id: Uuid) -> Result<Vec<AgentPresence>, ClientError> {
        let res = self
            .request(reqwest::Method::GET, &format!("/api/channels/{id}/agents"))
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }
}
