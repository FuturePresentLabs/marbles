//! One outbox, one sink: the loop that turns pending events into deliveries.
//!
//! `log_event` writes the audit row and the outbox row in the same transaction, so
//! a crash can lose neither and a consumer can be told neither twice nor never.
//! This is the other half of that guarantee: take pending rows, hand each to the
//! configured sink, and record the outcome — delivered before the next event, not
//! in a batch at the end, so a failure is attributed to the event that caused it.
//!
//! The sink is a choice, not a chain. An outbox row holds one delivery state, so
//! two sinks would mark each other's work done; `server.webhook` and
//! `server.unibus` are alternatives and `main` refuses both.

use std::sync::Arc;
use std::time::Duration;

use crate::store::{CompanyStoreRegistry, OutboundEvent, Store, now};
use crate::unibus::UnibusSink;
use crate::webhook::{EventEnvelope, Publisher};

/// Where a batch of events goes.
pub enum Sink {
    Webhook(Publisher),
    Unibus(Arc<UnibusSink>),
}

impl Sink {
    /// Whether this sink publishes that event at all.
    ///
    /// An event the sink has no use for (`heartbeat`, `labels`) is still *handled*:
    /// the dispatcher marks it delivered. Leaving it pending would put a row it
    /// will never send at the head of the queue, and the queue is ordered.
    #[must_use]
    pub fn publishes(&self, event: &str) -> bool {
        match self {
            Sink::Webhook(_) => true,
            Sink::Unibus(sink) => sink.kind_for(event).is_some(),
        }
    }

    /// Which sink this is, for the startup line and error messages.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Sink::Webhook(_) => "webhook",
            Sink::Unibus(_) => "unibus",
        }
    }
}

pub struct Dispatcher {
    sink: Sink,
    default_db: Arc<dyn Store>,
    companies: Option<Arc<dyn CompanyStoreRegistry>>,
}

impl Dispatcher {
    #[must_use]
    pub fn new(
        sink: Sink,
        default_db: Arc<dyn Store>,
        companies: Option<Arc<dyn CompanyStoreRegistry>>,
    ) -> Self {
        Self {
            sink,
            default_db,
            companies,
        }
    }

    pub async fn run(self) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            if let Err(error) = self.deliver_once().await {
                eprintln!("marbles {} dispatcher: {error}", self.sink.label());
            }
        }
    }

    async fn deliver_once(&self) -> Result<(), String> {
        let stores = match &self.companies {
            Some(companies) => companies.open_stores().map_err(|e| e.to_string())?,
            None => vec![("local".to_string(), Arc::clone(&self.default_db))],
        };
        for (company, db) in stores {
            let pending_db = Arc::clone(&db);
            let events = tokio::task::spawn_blocking(move || pending_db.pending_events(now(), 64))
                .await
                .map_err(|_| "outbox storage worker failed".to_string())?
                .map_err(|e| e.to_string())?;
            for event in events {
                let outcome = if self.sink.publishes(&event.event) {
                    let envelope = {
                        let db = Arc::clone(&db);
                        let event = event.clone();
                        let company = company.clone();
                        tokio::task::spawn_blocking(move || envelope_for(&*db, &company, &event))
                            .await
                            .map_err(|_| "outbox enrichment worker failed".to_string())?
                    };
                    self.deliver(&event, &envelope).await
                } else {
                    Ok(())
                };
                let update_db = Arc::clone(&db);
                tokio::task::spawn_blocking(move || match outcome {
                    Ok(()) => update_db.mark_event_delivered(event.seq, now()),
                    Err(error) => {
                        let exponent = event.attempts.clamp(0, 8) as u32;
                        let delay = 1_i64 << exponent;
                        update_db.mark_event_failed(event.seq, now() + delay, &error)
                    }
                })
                .await
                .map_err(|_| "outbox update worker failed".to_string())?
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    async fn deliver(&self, event: &OutboundEvent, envelope: &EventEnvelope) -> Result<(), String> {
        match &self.sink {
            Sink::Webhook(publisher) => publisher.post(envelope).await,
            Sink::Unibus(sink) => {
                let kind = sink
                    .kind_for(&event.event)
                    .ok_or_else(|| format!("no kind for event {:?}", event.event))?;
                let sink = Arc::clone(sink);
                let envelope = envelope.clone();
                tokio::task::spawn_blocking(move || sink.publish(kind, &envelope))
                    .await
                    .map_err(|_| "unibus worker failed".to_string())?
            }
        }
    }
}

/// The envelope a sink delivers: the outbox row's machine facts plus the words the
/// reader needs — the audit row's `actor`/`detail`, and the issue title.
///
/// Both lookups are best-effort. A notification that lost its title still names the
/// issue, and a store hiccup during enrichment must not turn into a delivery retry
/// storm over a message that would have been understandable anyway.
fn envelope_for(store: &dyn Store, company: &str, event: &OutboundEvent) -> EventEnvelope {
    let mut envelope = EventEnvelope::from_event(company, event);
    match store.event_context(event.seq) {
        Ok(Some(context)) => {
            envelope.actor = context.actor;
            envelope.detail = context.detail;
        }
        Ok(None) => {}
        Err(error) => eprintln!(
            "marbles outbox: event {} has no audit row ({error})",
            event.seq
        ),
    }
    if let Ok(issue) = store.get(&event.issue_id) {
        envelope.title = issue.title;
    }
    envelope
}
