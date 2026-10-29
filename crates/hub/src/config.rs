#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubMode {
    Hosted,
    Open,
}

#[derive(Debug, Clone)]
pub struct HubConfig {
    pub database_url: String,
    pub hub_mode: HubMode,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub public_url: String,
    /// Test override: serves /auth, /token and /userinfo under one base. None uses the real Google hosts.
    pub google_base_url: Option<String>,
}

impl HubConfig {
    pub fn from_env() -> HubConfig {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let hub_mode = match var("HUB_MODE").as_str() {
            "hosted" => HubMode::Hosted,
            _ => HubMode::Open,
        };
        HubConfig {
            database_url: var("DATABASE_URL"),
            hub_mode,
            google_client_id: var("GOOGLE_CLIENT_ID"),
            google_client_secret: var("GOOGLE_CLIENT_SECRET"),
            public_url: std::env::var("PUBLIC_URL")
                .unwrap_or_else(|_| "http://localhost:8080".into()),
            google_base_url: std::env::var("GOOGLE_BASE_URL").ok(),
        }
    }

    pub fn google_auth_url(&self) -> String {
        match &self.google_base_url {
            Some(b) => format!("{b}/auth"),
            None => "https://accounts.google.com/o/oauth2/v2/auth".into(),
        }
    }

    pub fn google_token_url(&self) -> String {
        match &self.google_base_url {
            Some(b) => format!("{b}/token"),
            None => "https://oauth2.googleapis.com/token".into(),
        }
    }

    pub fn google_userinfo_url(&self) -> String {
        match &self.google_base_url {
            Some(b) => format!("{b}/userinfo"),
            None => "https://openidconnect.googleapis.com/v1/userinfo".into(),
        }
    }
}
