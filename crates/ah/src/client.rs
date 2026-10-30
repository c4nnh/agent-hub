use ah_common::{
    AgentPresence, AuthPollResponse, ChannelSummary, ContextEntry, ContextEntrySummary, ContextHit,
    DeliveryState, InvitationSummary, MeResponse, Message, PostMessage, SenderType, UpsertContext,
};
use reqwest::{Response, StatusCode};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("hub returned {status}: {message}")]
    Status { status: u16, message: String },
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
}

#[derive(Clone)]
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
            // No hub call is a long poll, so a stalled request is a dead connection.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("http client"),
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

impl HubClient {
    pub async fn delivery(&self, id: Uuid) -> Result<DeliveryState, ClientError> {
        let res = self
            .request(
                reqwest::Method::GET,
                &format!("/api/channels/{id}/delivery"),
            )
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    /// Tells the hub how far this agent's client has delivered. Best effort: a slow or failing
    /// hub must never hold up delivery, so errors and the 2s timeout are dropped.
    pub async fn report_agent_cursor(&self, channel: Uuid, agent_name: &str, last_read_id: i64) {
        let sent = self
            .request(
                reqwest::Method::PUT,
                &format!("/api/channels/{channel}/agent-cursor"),
            )
            .timeout(std::time::Duration::from_secs(2))
            .json(&serde_json::json!({ "agent_name": agent_name, "last_read_id": last_read_id }))
            .send()
            .await;
        drop(sent);
    }

    /// Daemon heartbeat: tells the hub this host covers agents ending in `suffix`. Best effort
    /// like `report_agent_cursor`: errors and the 2s timeout are dropped.
    pub async fn report_coverage(&self, channel: Uuid, suffix: &str) {
        let sent = self
            .request(
                reqwest::Method::PUT,
                &format!("/api/channels/{channel}/coverage"),
            )
            .timeout(std::time::Duration::from_secs(2))
            .json(&serde_json::json!({ "suffix": suffix }))
            .send()
            .await;
        drop(sent);
    }

    /// `report_agent_cursor` for every channel, concurrently.
    pub async fn report_agent_cursors(
        &self,
        agent_name: &str,
        cursors: &std::collections::BTreeMap<Uuid, i64>,
    ) {
        let reports = cursors
            .iter()
            .map(|(id, cursor)| self.report_agent_cursor(*id, agent_name, *cursor));
        futures_util::future::join_all(reports).await;
    }

    /// `ws(s)://` URL for the hub's websocket endpoint, with the token as query parameter.
    pub fn ws_url(&self) -> Result<String, ClientError> {
        let mut url =
            reqwest::Url::parse(&self.url("/api/ws")).map_err(|e| ClientError::Status {
                status: 0,
                message: e.to_string(),
            })?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        // Cannot fail: http(s) -> ws(s) are both special schemes.
        let _ = url.set_scheme(scheme);
        if let Some(t) = &self.token {
            url.query_pairs_mut().append_pair("token", t);
        }
        Ok(url.to_string())
    }

    /// Messages with id > `since_id`, ascending (the hub caps one page at 500).
    pub async fn messages(
        &self,
        channel_id: Uuid,
        since_id: i64,
        limit: i64,
    ) -> Result<Vec<Message>, ClientError> {
        let res = self
            .request(
                reqwest::Method::GET,
                &format!("/api/channels/{channel_id}/messages"),
            )
            .query(&[("since_id", since_id), ("limit", limit)])
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    pub async fn post_human_message(
        &self,
        channel_id: Uuid,
        sender_name: &str,
        body: &str,
    ) -> Result<Message, ClientError> {
        let msg = PostMessage {
            body: body.to_string(),
            reply_to: None,
            mentions: vec![],
            sender_type: SenderType::Human,
            sender_name: sender_name.to_string(),
        };
        self.post_message(channel_id, &msg).await
    }

    pub async fn post_message(
        &self,
        channel_id: Uuid,
        msg: &PostMessage,
    ) -> Result<Message, ClientError> {
        let res = self
            .request(
                reqwest::Method::POST,
                &format!("/api/channels/{channel_id}/messages"),
            )
            .json(msg)
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }
}

impl HubClient {
    pub async fn list_context(
        &self,
        channel_id: Uuid,
    ) -> Result<Vec<ContextEntrySummary>, ClientError> {
        let res = self
            .request(
                reqwest::Method::GET,
                &format!("/api/channels/{channel_id}/context"),
            )
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    pub async fn get_context(&self, id: Uuid) -> Result<ContextEntry, ClientError> {
        let res = self
            .request(reqwest::Method::GET, &format!("/api/context/{id}"))
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    /// A stale `base_version` fails with status 409 and the current entry as JSON message.
    pub async fn upsert_context(
        &self,
        channel_id: Uuid,
        req: &UpsertContext,
    ) -> Result<ContextEntry, ClientError> {
        let res = self
            .request(
                reqwest::Method::POST,
                &format!("/api/channels/{channel_id}/context"),
            )
            .json(req)
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }

    pub async fn search_context(
        &self,
        channel_id: Uuid,
        query: &str,
        k: Option<i64>,
    ) -> Result<Vec<ContextHit>, ClientError> {
        let res = self
            .request(
                reqwest::Method::POST,
                &format!("/api/channels/{channel_id}/context/search"),
            )
            .json(&serde_json::json!({ "query": query, "k": k }))
            .send()
            .await?;
        Ok(Self::check(res).await?.json().await?)
    }
}
