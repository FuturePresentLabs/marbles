//! The thin HTTP client the CLI uses when a server URL is configured. One op per POST, bearer
//! token, plain-text error bodies that map straight onto process stderr — the transport is meant
//! to be boring enough to debug by reading once.

use serde::Serialize;

pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// POST /v1/<op> with a JSON body; returns the parsed response or an error that carries the
    /// server's text so callers can print `HTTP 409: issue X is already claimed by Y`.
    pub async fn call<T: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        body: &impl Serialize,
    ) -> Result<T, String> {
        let response = self
            .http
            .post(format!("{}/v1/{op}", self.base))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("connecting to {}: {e}", self.base))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {}: {}", status.as_u16(), text.trim()));
        }
        serde_json::from_str(&text).map_err(|e| format!("parsing {op} response: {e}\n{text}"))
    }

    pub async fn health(&self) -> bool {
        self.http
            .get(format!("{}/healthz", self.base))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}
