//! The thin HTTP client the CLI uses when a server URL is configured. One op per POST, bearer
//! token, plain-text error bodies that map straight onto process stderr — the transport is meant
//! to be boring enough to debug by reading once.
//!
//! Two rules keep a tracker outage from stopping the fleet:
//!
//! - every call is bounded by a timeout (total and connect), so a hung server fails in seconds;
//! - successful *reads* are cached locally and replayed, clearly labelled, when the server is
//!   unreachable or 5xx. Writes are never cached and never queued: they fail loudly and non-zero.

use std::time::Duration;

use serde::Serialize;

use crate::cache::{self, Cache};

/// Default total request deadline. Overridable with `--timeout-ms` / `MARBLES_TIMEOUT_MS`.
pub const DEFAULT_TIMEOUT_MS: u64 = 8_000;
/// A connect attempt gets at most this long, even if the total deadline is larger.
const CONNECT_TIMEOUT_MS: u64 = 3_000;

/// Ops that only read the tracker; these alone are eligible for the offline cache. The CLI keeps
/// this list 1:1 with its read commands (`list`, `ready`, `show`, `history`, `stats`, `projects`).
pub const READ_OPS: &[&str] = &[
    "issues.list",
    "issues.ready",
    "issues.get",
    "history.get",
    "stats",
    "projects.list",
    "views.dashboard",
    "views.scrum",
    "views.gantt",
];

pub fn is_read_op(op: &str) -> bool {
    READ_OPS.contains(&op)
}

/// How the client is wired for one CLI invocation.
#[derive(Default, Clone)]
pub struct Options {
    /// Total per-request deadline; `None` means [`DEFAULT_TIMEOUT_MS`].
    pub timeout: Option<Duration>,
    /// Local read cache; `None` disables offline reads (local mode never sets it).
    pub cache: Option<Cache>,
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
    cache: Option<Cache>,
}

/// Why a request did not produce a response body.
enum Failure {
    /// No usable answer: transport error, timeout, or 5xx. A write did not apply.
    Unavailable(String),
    /// The server answered and refused (4xx, or an unparsable body).
    Rejected(String),
}

impl Client {
    pub fn new(base: &str, token: &str) -> Self {
        Self::with_options(base, token, Options::default())
    }

    pub fn with_options(base: &str, token: &str, options: Options) -> Self {
        let timeout = options
            .timeout
            .unwrap_or(Duration::from_millis(DEFAULT_TIMEOUT_MS));
        let http = reqwest::Client::builder()
            .connect_timeout(timeout.min(Duration::from_millis(CONNECT_TIMEOUT_MS)))
            .timeout(timeout)
            // Credentials must never follow a redirect to an unintended host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            cache: options.cache,
        }
    }

    /// POST /v1/<op> with a JSON body; returns the parsed response or an error that carries the
    /// server's text so callers can print `HTTP 409: issue X is already claimed by Y`.
    ///
    /// Failures that mean "the tracker is unavailable" are handled by op kind: reads fall back to
    /// the newest cached response (banner on stderr), writes return an explicit not-applied error.
    pub async fn call<T: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        body: &impl Serialize,
    ) -> Result<T, String> {
        let value =
            serde_json::to_value(body).map_err(|e| format!("serializing {op} request: {e}"))?;
        let key = serde_json::to_string(&value).unwrap_or_default();
        let cacheable = is_read_op(op);
        match self.send(op, &value).await {
            Ok(text) => {
                if cacheable && let Some(cache) = &self.cache {
                    // Best effort: a cache that cannot be written must not fail the read.
                    let _ = cache.store(&self.base, op, &key, &text);
                }
                parse(op, &text)
            }
            Err(Failure::Rejected(message)) => Err(message),
            Err(Failure::Unavailable(reason)) => {
                if !cacheable {
                    return Err(format!("offline: {op} not applied (no server): {reason}"));
                }
                if let Some(cache) = &self.cache
                    && let Some(cached) = cache.load(&self.base, op, &key)
                {
                    let age = now_seconds() - cached.fetched_at;
                    eprintln!(
                        "marbles: offline (cached {}, {} old, from {})",
                        cache::format_timestamp(cached.fetched_at),
                        cache::humanize_age(age),
                        self.base
                    );
                    eprintln!("marbles: {reason}");
                    return parse(op, &cached.body);
                }
                Err(format!(
                    "offline: no cached {op} response for {} ({reason})",
                    self.base
                ))
            }
        }
    }

    async fn send(&self, op: &str, body: &serde_json::Value) -> Result<String, Failure> {
        let response = self
            .http
            .post(format!("{}/v1/{op}", self.base))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| Failure::Unavailable(format!("connecting to {}: {e}", self.base)))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| Failure::Unavailable(format!("reading {} response: {e}", self.base)))?;
        if status.is_success() {
            return Ok(text);
        }
        let detail = format!("HTTP {}: {}", status.as_u16(), text.trim());
        if status.is_server_error() {
            // A 5xx is "the tracker is broken", not "your request was wrong": reads may replay.
            Err(Failure::Unavailable(detail))
        } else {
            Err(Failure::Rejected(detail))
        }
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

fn parse<T: serde::de::DeserializeOwned>(op: &str, text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|e| format!("parsing {op} response: {e}\n{text}"))
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_ops_are_a_closed_list() {
        for op in READ_OPS {
            assert!(is_read_op(op));
        }
        for op in [
            "issues.create",
            "issues.update",
            "issues.close",
            "issues.supersede",
            "issues.import",
            "deps.add",
            "labels.remove",
            "claims.claim",
            "claims.touch",
            "claims.release",
            "claims.sweep",
            "projects.ensure",
        ] {
            assert!(!is_read_op(op), "{op} must never be served from cache");
        }
    }

    #[test]
    fn builder_always_has_a_timeout() {
        let client = Client::new("https://marbles.fpl.dev/", "token");
        assert_eq!(client.base, "https://marbles.fpl.dev");
    }
}
