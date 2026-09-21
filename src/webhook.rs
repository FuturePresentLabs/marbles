//! Durable signed publication of tracker mutations.
//!
//! The database transaction that records history also appends the outbox row. HTTP delivery is
//! deliberately asynchronous: an unavailable consumer never makes a Marble mutation fail, and
//! the consumer's ordinary reconciliation poll remains the recovery path.

use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;

use crate::config::WebhookConfig;
use crate::db::{CompanyStores, Db, OutboundEvent, now};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Serialize)]
struct Envelope<'a> {
    schema: &'static str,
    id: String,
    company: &'a str,
    project: &'a str,
    issue_id: &'a str,
    event: &'a str,
    occurred_at: i64,
}

pub struct Publisher {
    config: WebhookConfig,
    secret: Vec<u8>,
    client: reqwest::Client,
    default_db: Arc<Db>,
    companies: Option<Arc<CompanyStores>>,
}

impl Publisher {
    pub fn new(
        config: WebhookConfig,
        default_db: Arc<Db>,
        companies: Option<Arc<CompanyStores>>,
    ) -> Result<Self, String> {
        let secret = std::fs::read(&config.secret_file).map_err(|e| {
            format!(
                "reading webhook secret {}: {e}",
                config.secret_file.display()
            )
        })?;
        let secret = secret.strip_suffix(b"\n").unwrap_or(&secret).to_vec();
        if secret.len() < 32 {
            return Err("webhook secret must contain at least 32 bytes".into());
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_seconds))
            .build()
            .map_err(|e| format!("building webhook client: {e}"))?;
        Ok(Self {
            config,
            secret,
            client,
            default_db,
            companies,
        })
    }

    pub async fn run(self) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            if let Err(error) = self.deliver_once().await {
                eprintln!("marbles webhook dispatcher: {error}");
            }
        }
    }

    async fn deliver_once(&self) -> Result<(), String> {
        let stores = match &self.companies {
            Some(companies) => companies.open_stores().map_err(|e| e.to_string())?,
            None => vec![("local".to_string(), Arc::clone(&self.default_db))],
        };
        for (company, db) in stores {
            for event in db.pending_events(now(), 64).map_err(|e| e.to_string())? {
                match self.deliver(&company, &event).await {
                    Ok(()) => db
                        .mark_event_delivered(event.seq, now())
                        .map_err(|e| e.to_string())?,
                    Err(error) => {
                        let exponent = event.attempts.clamp(0, 8) as u32;
                        let delay = 1_i64 << exponent;
                        db.mark_event_failed(event.seq, now() + delay, &error)
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn deliver(&self, company: &str, event: &OutboundEvent) -> Result<(), String> {
        let envelope = Envelope {
            schema: "marbles.event/1",
            id: format!("{company}:{}", event.seq),
            company,
            project: &event.project,
            issue_id: &event.issue_id,
            event: &event.event,
            occurred_at: event.occurred_at,
        };
        let body = serde_json::to_vec(&envelope).map_err(|e| e.to_string())?;
        let signature = sign(&self.secret, &body)?;
        let response = self
            .client
            .post(&self.config.url)
            .header("content-type", "application/json")
            .header("x-marbles-signature", format!("sha256={signature}"))
            .body(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("receiver returned {}", response.status()))
        }
    }
}

fn sign(secret: &[u8], body: &[u8]) -> Result<String, String> {
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|e| e.to_string())?;
    mac.update(body);
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_stable_and_body_bound() {
        let secret = b"01234567890123456789012345678901";
        assert_eq!(sign(secret, b"one").unwrap(), sign(secret, b"one").unwrap());
        assert_ne!(sign(secret, b"one").unwrap(), sign(secret, b"two").unwrap());
    }
}
