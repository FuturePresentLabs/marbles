//! The producer half of `soil:unibus`: Marbles' outbox, on the mesh.
//!
//! Publishing is not something a mutation can wait on, so the outbox row is the
//! durable record and this sink is the transport. One registered session is reused
//! across events; a failed write drops it, and the next delivery re-registers
//! rather than retrying a session whose registration state is already suspect.
//!
//! The hub routes *kinds*, so the mapping here is the contract with every consumer:
//! only the four events a human can act on get a kind. `heartbeat`, `labels`, and
//! `updated` stay in the audit trail — publishing them would train the reader to
//! ignore the channel.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;

#[cfg(not(target_arch = "wasm32"))]
use serde_json::json;
#[cfg(not(target_arch = "wasm32"))]
use unibus::Message;
#[cfg(not(target_arch = "wasm32"))]
use unibus::client::{Client, ClientConfig};

#[cfg(not(target_arch = "wasm32"))]
use crate::config::UnibusConfig;
#[cfg(not(target_arch = "wasm32"))]
use crate::webhook::EventEnvelope;

// The kinds the hub must grant this producer. They are versioned: a consumer
// written against `created.v1` keeps working when a `v2` body appears.
pub const CREATED_KIND: &str = "marbles.issue.created.v1";
pub const CLAIMED_KIND: &str = "marbles.issue.claimed.v1";
pub const STATUS_KIND: &str = "marbles.issue.status.v1";
pub const CLOSED_KIND: &str = "marbles.issue.closed.v1";

/// The kind an audit event publishes as; `None` means "not a notification".
///
/// A claim gets its own kind instead of riding `status`: claiming does not change
/// status, and a consumer that wants "who is on this" should not have to parse a
/// status transition to find out.
#[must_use]
pub fn kind_for(event: &str) -> Option<&'static str> {
    match event {
        "created" => Some(CREATED_KIND),
        "claimed" => Some(CLAIMED_KIND),
        "status" => Some(STATUS_KIND),
        "closed" => Some(CLOSED_KIND),
        _ => None,
    }
}

/// A registered hub session, reconnecting on demand.
#[cfg(not(target_arch = "wasm32"))]
pub struct UnibusSink {
    config: ClientConfig,
    client: Mutex<Option<Client>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl UnibusSink {
    /// Validates everything that can be checked without the hub being up: the
    /// endpoint parses, a zone is served, and the token is readable. The hub's
    /// answer to the registration is only available at the first publish, so a bad
    /// grant still surfaces as a delivery error — but a missing token file fails
    /// here, at startup, while someone is watching.
    pub fn new(config: &UnibusConfig) -> Result<Self, String> {
        if config.zones.is_empty() {
            return Err("server.unibus.zones must name at least one zone".to_string());
        }
        let client_config: ClientConfig = serde_json::from_value(json!({
            "installation_id": config.installation_id,
            "producer_id": config.producer_id,
            "hub_endpoint": config.hub_endpoint,
            "token_file": config.token_file,
            "zones": config.zones,
        }))
        .map_err(|error| format!("server.unibus: {error}"))?;
        client_config
            .token()
            .map_err(|error| format!("server.unibus: {error}"))?;
        if client_config
            .hub_endpoint
            .parse::<std::net::SocketAddr>()
            .is_err()
        {
            return Err(format!(
                "server.unibus.hub_endpoint must be ip:port, got {:?}",
                client_config.hub_endpoint
            ));
        }
        Ok(Self {
            config: client_config,
            client: Mutex::new(None),
        })
    }

    #[must_use]
    pub fn kind_for(&self, event: &str) -> Option<&'static str> {
        kind_for(event)
    }

    /// Publish one envelope as `kind`. `Ok` means the frame was written, not that a
    /// consumer acted on it: the bus is one-way by design.
    pub fn publish(&self, kind: &str, envelope: &EventEnvelope) -> Result<(), String> {
        let body = serde_json::to_value(envelope).map_err(|error| error.to_string())?;
        let mut guard = self
            .client
            .lock()
            .map_err(|_| "unibus: session lock poisoned".to_string())?;
        if guard.is_none() {
            let client = Client::connect(self.config.clone())
                .map_err(|error| format!("connect hub at {}: {error}", self.config.hub_endpoint))?;
            *guard = Some(client);
        }
        let sent = {
            let client = guard
                .as_ref()
                .ok_or_else(|| "unibus: session missing after connect".to_string())?;
            client
                .envelope(None, Message::new(kind, body))
                .and_then(|frame| client.send(&frame, &[]))
        };
        if sent.is_err() {
            // Registration state is per-session; a half-written session must not be
            // reused, so forget it and let the retry start clean.
            *guard = None;
        }
        sent.map_err(|error| format!("publish {kind}: {error}"))
    }
}
