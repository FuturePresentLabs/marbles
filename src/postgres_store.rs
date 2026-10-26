//! Hosted PostgreSQL storage. Every statement carries `company_id`; the tenant
//! boundary is data, not connection state or a caller-controlled search path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use postgres::{Client, GenericClient, Transaction};
use postgres_openssl::MakeTlsConnector;

use crate::db::{
    CompanyStoreRegistry, Error, OutboundEvent, Result, Store, SweepReport, now,
    validate_company_id,
};
use crate::time::WorkWeek;
use crate::types::*;

pub struct PostgresCompanyStores {
    client: Arc<SharedClient>,
    open: Mutex<BTreeMap<String, Arc<dyn Store>>>,
}

impl PostgresCompanyStores {
    pub fn connect(database_url: &str) -> Result<Self> {
        let connector = crate::postgres_migration::tls_connector()
            .map_err(|e| Error::Bad(format!("building PostgreSQL TLS connector: {e}")))?;
        let client = connect_bounded(database_url, &connector)?;
        Ok(Self {
            client: Arc::new(SharedClient {
                url: database_url.to_owned(),
                connector,
                state: Mutex::new(SlotState {
                    client: Some(client),
                    reconnecting: false,
                }),
            }),
            open: Mutex::new(BTreeMap::new()),
        })
    }
}

/// Pause before re-dialing a connection the server dropped. Deliberately short and bounded:
/// this repairs a transient blip; it is not an unbounded retry loop and never runs while a
/// statement is in flight.
const RECONNECT_PAUSE: Duration = Duration::from_millis(200);

/// Bound the TCP connect. A half-open socket or an idle pooler drop can leave `connect` waiting
/// on a peer that will never answer; without this the shared client wedges forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Even a successful TCP handshake can stall in TLS/startup against a black-holed peer. The
/// sync client's `connect_timeout` only covers the TCP connect itself, so dial on a worker and
/// abandon it after this deadline rather than blocking the caller indefinitely.
const DIAL_DEADLINE: Duration = Duration::from_secs(6);

/// Notice a peer that vanished without sending a FIN during a long idle period. PostgreSQL's
/// own TCP keepalives are not guaranteed to be enabled server-side.
const KEEPALIVES_IDLE: Duration = Duration::from_secs(30);

/// Bound a statement the server never answers (e.g. a half-open socket after the query was
/// written). This is per statement, so a legitimately long transaction is not capped.
const STATEMENT_TIMEOUT_MS: u64 = 15_000;

/// Build the reconnect configuration: the caller's `DATABASE_URL` (including `sslmode=require`)
/// plus a bounded connect, keepalives, and a server-side statement timeout.
fn bounded_config(url: &str) -> Result<postgres::Config> {
    let mut config: postgres::Config = url
        .parse()
        .map_err(|e| Error::Bad(format!("parsing DATABASE_URL: {e}")))?;
    if config
        .get_connect_timeout()
        .is_none_or(|timeout| *timeout > CONNECT_TIMEOUT)
    {
        config.connect_timeout(CONNECT_TIMEOUT);
    }
    config.keepalives_idle(KEEPALIVES_IDLE);
    let statement_timeout = format!("-c statement_timeout={STATEMENT_TIMEOUT_MS}");
    let options = match config.get_options() {
        Some(existing) if !existing.trim().is_empty() => {
            format!("{existing} {statement_timeout}")
        }
        _ => statement_timeout,
    };
    config.options(&options);
    Ok(config)
}

/// Connect with the bounded configuration and ensure the schema exists on the fresh handle.
/// The dial runs on a worker thread so the caller can abandon a stalled TLS/startup handshake
/// after [`DIAL_DEADLINE`]; a late connection is simply dropped when its sender is gone.
fn connect_bounded(url: &str, connector: &MakeTlsConnector) -> Result<Client> {
    let config = bounded_config(url)?;
    let connector = connector.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut client = match config.connect(connector) {
            Ok(client) => client,
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        };
        let installed = crate::postgres_migration::install_schema(&mut client);
        let _ = sender.send(installed.map(|()| client));
    });
    match receiver.recv_timeout(DIAL_DEADLINE) {
        Ok(result) => Ok(result?),
        Err(_) => Err(Error::Bad(format!(
            "connecting to PostgreSQL timed out after {DIAL_DEADLINE:?}"
        ))),
    }
}

/// SQLSTATEs that mean "this connection is gone", not "your statement is wrong". 08xxx is the
/// connection-exception class, 57P0x is administrative shutdown/crash recovery, and 53300 is
/// too many connections, which a later re-dial can also clear.
fn is_disconnect_sqlstate(code: Option<&str>) -> bool {
    matches!(
        code,
        Some(
            "08000" | "08001" | "08003" | "08004" | "08006" | "57P01" | "57P02" | "57P03" | "53300"
        )
    )
}

/// A killed backend does not always deliver a FATAL message first: an idle socket can surface
/// as a bare TCP reset. Treat those IO kinds as connection loss too, otherwise a real restart
/// leaves the process permanently dead (the exact incident this fixes).
fn is_disconnect_io_kind(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind;
    matches!(
        kind,
        ErrorKind::BrokenPipe
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof
    )
}

fn is_disconnect(error: &postgres::Error) -> bool {
    if error.is_closed() || is_disconnect_sqlstate(error.code().map(|code| code.code())) {
        return true;
    }
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return is_disconnect_io_kind(io.kind());
        }
        source = cause.source();
    }
    false
}

/// The state behind the shared-client mutex. `reconnecting` is set (and the lock released)
/// while a replacement connection is being dialed, so concurrent callers fail fast instead of
/// queueing behind a connect that may never finish.
struct SlotState<C> {
    client: Option<C>,
    reconnecting: bool,
}

/// Run `op` against a live connection, replacing the connection exactly once if the server
/// dropped it. The slot mutex serializes statements (the single-writer model) but is **never**
/// held across a dial: while `reconnecting` is set other callers get `reconnecting()` instead
/// of blocking, which is what keeps a stalled reconnect from wedging `/healthz`. On a failed
/// reconnect the slot stays empty, so the next call tries again instead of pinning a dead
/// handle forever.
///
/// `op` must be repeatable for the error classes `retryable` accepts: a single statement is
/// rolled back by the dropped connection, and a transaction only retries failures raised
/// before `COMMIT` was attempted (see `PostgresStore::transaction`).
fn with_reconnect<C, T, E>(
    slot: &Mutex<SlotState<C>>,
    mut reconnect: impl FnMut() -> std::result::Result<C, E>,
    mut op: impl FnMut(&mut C) -> std::result::Result<T, E>,
    retryable: impl Fn(&E) -> bool,
    pause: Duration,
    reconnecting: impl Fn() -> E,
) -> std::result::Result<T, E> {
    // A poisoned lock means an earlier panic, not a dead connection. Recover the guarded
    // connection rather than turning every later store call into an error.
    let mut state = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if state.reconnecting {
        return Err(reconnecting());
    }
    let mut client = match state.client.take() {
        Some(client) => client,
        None => {
            // Release the lock before dialing so a stalled connect cannot block every other
            // caller (and every health probe) behind this mutex.
            state.reconnecting = true;
            drop(state);
            let dialed = reconnect();
            state = slot.lock().unwrap_or_else(PoisonError::into_inner);
            state.reconnecting = false;
            dialed?
        }
    };
    match op(&mut client) {
        Ok(value) => {
            state.client = Some(client);
            Ok(value)
        }
        Err(error) if retryable(&error) => {
            // Drop the known-dead handle before pausing so nothing can reuse it.
            drop(client);
            state.reconnecting = true;
            drop(state);
            std::thread::sleep(pause);
            let dialed = reconnect();
            state = slot.lock().unwrap_or_else(PoisonError::into_inner);
            state.reconnecting = false;
            let mut fresh = dialed?;
            let result = op(&mut fresh);
            // Keep a healthy replacement; discard one that died during the retry so the next
            // call re-dials instead of retrying on a known-dead handle.
            if !matches!(&result, Err(error) if retryable(error)) {
                state.client = Some(fresh);
            }
            result
        }
        Err(error) => {
            state.client = Some(client);
            Err(error)
        }
    }
}

fn is_disconnect_error(error: &Error) -> bool {
    matches!(error, Error::Postgres(source) if is_disconnect(source))
}

/// The one connection shared by every store. It can be re-dialed; the single-writer model
/// (one connection serialized behind one mutex) is unchanged.
struct SharedClient {
    url: String,
    connector: MakeTlsConnector,
    state: Mutex<SlotState<Client>>,
}

impl SharedClient {
    fn with<T>(&self, op: impl FnMut(&mut Client) -> Result<T>) -> Result<T> {
        self.with_retry(op, is_disconnect_error)
    }

    fn with_retry<T>(
        &self,
        op: impl FnMut(&mut Client) -> Result<T>,
        retryable: impl Fn(&Error) -> bool,
    ) -> Result<T> {
        with_reconnect(
            &self.state,
            || connect_bounded(&self.url, &self.connector),
            op,
            retryable,
            RECONNECT_PAUSE,
            || {
                Error::Bad(
                    "PostgreSQL connection is being re-established; retry the request".into(),
                )
            },
        )
    }
}

impl CompanyStoreRegistry for PostgresCompanyStores {
    fn health_check(&self) -> Result<()> {
        // A real round-trip, not a cached flag: a dead connection is replaced and retried once
        // inside `SharedClient::with`, so /healthz recovers when PostgreSQL comes back.
        self.client.with(|client| {
            client.query("SELECT 1 FROM marbles_project LIMIT 1", &[])?;
            Ok(())
        })
    }

    fn for_company(&self, company_id: &str) -> Result<Arc<dyn Store>> {
        validate_company_id(company_id)?;
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(store) = open.get(company_id) {
            return Ok(Arc::clone(store));
        }
        let store: Arc<dyn Store> = Arc::new(PostgresStore {
            client: Arc::clone(&self.client),
            company_id: company_id.to_owned(),
            policy: Policy::default(),
            week: WorkWeek::default(),
        });
        open.insert(company_id.to_owned(), Arc::clone(&store));
        Ok(store)
    }

    fn sweep_open(&self, at: i64) -> Result<Vec<(String, SweepReport)>> {
        self.open_stores()?
            .into_iter()
            .map(|(company, store)| Ok((company, store.sweep(at)?)))
            .collect()
    }

    fn open_stores(&self) -> Result<Vec<(String, Arc<dyn Store>)>> {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(open
            .iter()
            .map(|(company, store)| (company.clone(), Arc::clone(store)))
            .collect())
    }
}

struct PostgresStore {
    client: Arc<SharedClient>,
    company_id: String,
    policy: Policy,
    week: WorkWeek,
}

impl PostgresStore {
    fn with_client<T>(&self, op: impl FnMut(&mut Client) -> Result<T>) -> Result<T> {
        self.client.with(op)
    }

    fn transaction<T>(&self, mut f: impl FnMut(&mut Transaction<'_>) -> Result<T>) -> Result<T> {
        // A COMMIT that loses the connection is ambiguous: the server may already have applied
        // it. Replaying the transaction could duplicate a state transition or its outbound
        // event, so only failures raised before COMMIT is attempted are retried.
        let commit_attempted = std::cell::Cell::new(false);
        self.client.with_retry(
            |client| {
                commit_attempted.set(false);
                let mut tx = client.transaction()?;
                // Serialize one tenant's state machine across blue/green server processes. This
                // also makes the migrated, per-company history sequence safe without introducing
                // a second sequence namespace that could collide with preserved SQLite ids.
                tx.execute(
                    "SELECT pg_advisory_xact_lock(hashtext($1))",
                    &[&self.company_id],
                )?;
                let value = f(&mut tx)?;
                commit_attempted.set(true);
                tx.commit()?;
                Ok(value)
            },
            |error| !commit_attempted.get() && is_disconnect_error(error),
        )
    }
}

impl Store for PostgresStore {
    fn ensure_project(&self, slug: &str, root: &str, prefix: &str) -> Result<()> {
        let at = now();
        self.transaction(|tx| {
            tx.execute(
                "INSERT INTO marbles_project(company_id,slug,root,prefix,created_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(company_id,slug) DO UPDATE SET root=excluded.root",
                &[&self.company_id, &slug, &root, &prefix, &at],
            )?;
            Ok(())
        })
    }

    fn projects(&self) -> Result<Vec<(String, String, String)>> {
        self.with_client(|client| {
            Ok(client
                .query(
                    "SELECT slug,root,prefix FROM marbles_project WHERE company_id=$1 ORDER BY slug",
                    &[&self.company_id],
                )?
                .into_iter()
                .map(|row| (row.get(0), row.get(1), row.get(2)))
                .collect())
        })
    }

    fn ensure_folder(&self, folder: &Folder, at: i64) -> Result<()> {
        let slug = folder.slug.trim();
        if slug.is_empty() {
            return Err(Error::Bad("folder slug is empty".into()));
        }
        let name = folder.name.trim();
        if name.is_empty() {
            return Err(Error::Bad("folder name is empty".into()));
        }
        let parent = folder
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        self.transaction(|tx| {
            if let Some(parent) = parent {
                if parent == slug {
                    return Err(Error::Bad("a folder cannot be its own parent".into()));
                }
                let exists = tx.query_opt(
                    "SELECT slug FROM marbles_folder WHERE company_id=$1 AND slug=$2",
                    &[&self.company_id, &parent],
                )?;
                if exists.is_none() {
                    return Err(Error::Bad(format!("no folder {parent}")));
                }
                let cycle = tx.query_opt(
                    "WITH RECURSIVE ancestors(slug) AS (
                         SELECT $2::text
                         UNION
                         SELECT f.parent FROM marbles_folder f
                           JOIN ancestors a ON f.slug = a.slug
                          WHERE f.company_id = $1 AND f.parent IS NOT NULL
                     )
                     SELECT slug FROM ancestors WHERE slug = $3 LIMIT 1",
                    &[&self.company_id, &parent, &slug],
                )?;
                if cycle.is_some() {
                    return Err(Error::Bad(format!(
                        "folder {slug} would become a descendant of itself"
                    )));
                }
            }
            tx.execute(
                "INSERT INTO marbles_folder(company_id,slug,name,parent,created_at)
                 VALUES($1,$2,$3,$4,$5)
                 ON CONFLICT(company_id,slug) DO UPDATE SET
                   name=excluded.name, parent=excluded.parent",
                &[&self.company_id, &slug, &name, &parent, &at],
            )?;
            Ok(())
        })
    }

    fn set_project_folder(&self, project: &str, folder: Option<&str>, at: i64) -> Result<()> {
        self.transaction(|tx| {
            let exists = tx.query_opt(
                "SELECT slug FROM marbles_project WHERE company_id=$1 AND slug=$2",
                &[&self.company_id, &project],
            )?;
            if exists.is_none() {
                return Err(Error::NoProject(project.to_string()));
            }
            match folder.map(str::trim).filter(|f| !f.is_empty()) {
                Some(folder) => {
                    let exists = tx.query_opt(
                        "SELECT slug FROM marbles_folder WHERE company_id=$1 AND slug=$2",
                        &[&self.company_id, &folder],
                    )?;
                    if exists.is_none() {
                        return Err(Error::Bad(format!("no folder {folder}")));
                    }
                    tx.execute(
                        "INSERT INTO marbles_project_folder(company_id,project,folder,assigned_at)
                         VALUES($1,$2,$3,$4)
                         ON CONFLICT(company_id,project) DO UPDATE SET
                           folder=excluded.folder, assigned_at=excluded.assigned_at",
                        &[&self.company_id, &project, &folder, &at],
                    )?;
                }
                None => {
                    tx.execute(
                        "DELETE FROM marbles_project_folder WHERE company_id=$1 AND project=$2",
                        &[&self.company_id, &project],
                    )?;
                }
            }
            Ok(())
        })
    }

    #[allow(clippy::field_reassign_with_default)]
    fn folder_view(&self) -> Result<FolderView> {
        self.with_client(|client| {
            let mut out = FolderView::default();
            out.folders = client
                .query(
                    "SELECT slug,name,parent,created_at FROM marbles_folder
                     WHERE company_id=$1 ORDER BY name,slug",
                    &[&self.company_id],
                )?
                .into_iter()
                .map(|r| Folder {
                    slug: r.get(0),
                    name: r.get(1),
                    parent: r.get(2),
                    created_at: r.get(3),
                })
                .collect();
            out.projects = client
                .query(
                    "SELECT project,folder FROM marbles_project_folder
                     WHERE company_id=$1 ORDER BY project",
                    &[&self.company_id],
                )?
                .into_iter()
                .map(|r| ProjectFolder {
                    project: r.get(0),
                    folder: r.get(1),
                })
                .collect();
            Ok(out)
        })
    }

    fn pending_events(&self, at: i64, limit: usize) -> Result<Vec<OutboundEvent>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_client(|client| {
            Ok(client.query("SELECT seq,store_id,issue_id,project,occurred_at,event,attempts FROM marbles_outbound_event WHERE company_id=$1 AND delivered_at IS NULL AND next_attempt_at <= $2 ORDER BY seq LIMIT $3", &[&self.company_id,&at,&limit])?.into_iter().map(|r| OutboundEvent { seq:r.get(0), store_id:r.get(1), issue_id:r.get(2), project:r.get(3), occurred_at:r.get(4), event:r.get(5), attempts:r.get(6) }).collect())
        })
    }

    fn mark_event_delivered(&self, seq: i64, at: i64) -> Result<()> {
        self.transaction(|tx| {
            tx.execute("UPDATE marbles_outbound_event SET delivered_at=$3,attempts=attempts+1,last_error=NULL WHERE company_id=$1 AND seq=$2 AND delivered_at IS NULL", &[&self.company_id,&seq,&at])?;
            Ok(())
        })
    }

    fn mark_event_failed(&self, seq: i64, next_attempt_at: i64, error: &str) -> Result<()> {
        self.transaction(|tx| {
            tx.execute("UPDATE marbles_outbound_event SET attempts=attempts+1,next_attempt_at=$3,last_error=$4 WHERE company_id=$1 AND seq=$2 AND delivered_at IS NULL", &[&self.company_id,&seq,&next_attempt_at,&error])?;
            Ok(())
        })
    }

    fn create(&self, spec: &NewIssue, at: i64) -> Result<String> {
        let project = spec
            .project
            .clone()
            .ok_or_else(|| Error::Bad("create requires a project".into()))?;
        let available = spec
            .available_at
            .unwrap_or(at + self.policy.creation_grace_seconds);
        self.transaction(|tx| {
            let prefix: String = tx.query_opt("SELECT prefix FROM marbles_project WHERE company_id=$1 AND slug=$2", &[&self.company_id,&project])?.ok_or_else(|| Error::NoProject(project.clone()))?.get(0);
            if let Some(parent) = spec.parent.as_deref() {
                let parent_project: Option<String> = tx
                    .query_opt(
                        "SELECT project FROM marbles_issue WHERE company_id=$1 AND id=$2",
                        &[&self.company_id, &parent],
                    )?
                    .map(|r| r.get(0));
                match parent_project {
                    None => return Err(Error::NotFound(parent.to_string())),
                    Some(p) if p != project => {
                        return Err(Error::Bad(format!(
                            "parent {parent} belongs to project {p}, not {project}; hierarchy is project-local (use a dependency edge for cross-project blockers)"
                        )))
                    }
                    Some(_) => {}
                }
            }
            let id = if let Some(id) = &spec.id {
                if tx.query_opt("SELECT 1 FROM marbles_issue WHERE company_id=$1 AND id=$2", &[&self.company_id,id])?.is_some() { return Err(Error::Bad(format!("issue id {id} already exists"))); }
                id.clone()
            } else { loop {
                let candidate = format!("{prefix}-{}", short_suffix());
                if tx.query_opt("SELECT 1 FROM marbles_issue WHERE company_id=$1 AND id=$2", &[&self.company_id,&candidate])?.is_none() { break candidate; }
            }};
            let labels = serde_json::to_string(&spec.labels)?;
            let mut metadata = match spec.metadata.clone().unwrap_or_else(|| serde_json::json!({})) { serde_json::Value::Object(map) => map, _ => return Err(Error::Bad("create metadata must be a JSON object".into())) };
            if let Some(reference)=&spec.external_ref { metadata.insert("external_ref".into(), serde_json::json!(reference)); }
            let metadata = serde_json::to_string(&metadata)?;
            let priority = if spec.priority == 0 { 2 } else { spec.priority };
            let created_by = spec.created_by.clone().unwrap_or_default();
            tx.execute("INSERT INTO marbles_issue(company_id,id,project,title,description,status,issue_type,priority,parent,labels_json,available_at,created_by,created_at,updated_at,metadata_json,external_ref) VALUES($1,$2,$3,$4,$5,'open',$6,$7,$8,$9,$10,$11,$12,$12,$13,$14)", &[&self.company_id,&id,&project,&spec.title,&spec.description,&spec.issue_type,&priority,&spec.parent,&labels,&available,&created_by,&at,&metadata,&spec.external_ref])?;
            log_event(tx,&self.company_id,&id,at,&created_by,"created",&spec.title)?;
            Ok(id)
        })
    }

    fn get(&self, id: &str) -> Result<Issue> {
        self.with_client(|client| {
            row_to_issue(&mut *client, &self.company_id, id)?
                .ok_or_else(|| Error::NotFound(id.into()))
        })
    }

    fn list(&self, project: Option<&str>, all: bool, only_id: Option<&str>) -> Result<Vec<Issue>> {
        self.with_client(|client| {
            let rows = client.query("SELECT id FROM marbles_issue WHERE company_id=$1 AND ($2::boolean OR status != 'closed') AND ($3::text IS NULL OR project=$3) AND ($4::text IS NULL OR id=$4) ORDER BY priority,created_at,id", &[&self.company_id,&all,&project,&only_id])?;
            rows.into_iter()
                .map(|r| {
                    let id: String = r.get(0);
                    row_to_issue(&mut *client, &self.company_id, &id)?.ok_or(Error::NotFound(id))
                })
                .collect()
        })
    }

    fn ready(&self, project: Option<&str>, at: i64) -> Result<Vec<Issue>> {
        self.with_client(|client| {
            let rows=client.query("SELECT i.id FROM marbles_issue i WHERE i.company_id=$1 AND i.status='open' AND (i.available_at IS NULL OR i.available_at <= $2) AND ($3::text IS NULL OR i.project=$3) AND NOT EXISTS(SELECT 1 FROM marbles_dep d JOIN marbles_issue p ON p.company_id=d.company_id AND p.id=d.depends_on WHERE d.company_id=i.company_id AND d.issue_id=i.id AND p.status!='closed') AND (i.assignee IS NULL OR (i.expires_at IS NOT NULL AND i.expires_at <= $2)) ORDER BY i.priority,i.created_at,i.id", &[&self.company_id,&at,&project])?;
            rows.into_iter()
                .map(|r| {
                    let id: String = r.get(0);
                    row_to_issue(&mut *client, &self.company_id, &id)?.ok_or(Error::NotFound(id))
                })
                .collect()
        })
    }

    fn update(&self, id: &str, patch: &IssuePatch, actor: &str, at: i64) -> Result<Issue> {
        if let Some(status) = &patch.status
            && !STATUSES.contains(&status.as_str())
        {
            return Err(Error::BadStatus(status.clone()));
        }
        self.transaction(|tx| {
            let mut issue=row_to_issue(tx,&self.company_id,id)?.ok_or_else(|| Error::NotFound(id.into()))?;
            if let Some(v)=&patch.title { issue.title=v.clone(); }
            if let Some(v)=&patch.description { issue.description=v.clone(); }
            if let Some(v)=&patch.append_notes { if !issue.description.is_empty(){issue.description.push('\n');} issue.description.push_str(v); }
            if let Some(v)=patch.priority { issue.priority=v; }
            if let Some(v)=&patch.evidence { issue.evidence=v.clone(); }
            if let Some(v)=&patch.metadata { let serde_json::Value::Object(incoming)=v else{return Err(Error::Bad("metadata patch must be a JSON object".into()))}; let mut map=issue.metadata.as_object().cloned().unwrap_or_default(); for (k,v) in incoming { if v.is_null(){map.remove(k);}else{map.insert(k.clone(),v.clone());}} issue.metadata=serde_json::Value::Object(map); issue.external_ref=issue.metadata.get("external_ref").and_then(|v|v.as_str()).map(str::to_owned); }
            let status_changed=patch.status.as_deref().is_some_and(|s|s!=issue.status); if let Some(v)=&patch.status {issue.status=v.clone();issue.closed_at=(v=="closed").then_some(at);}
            let available=patch.available_at.unwrap_or_else(|| (at+self.policy.edit_deferral_seconds).max(issue.available_at.unwrap_or(0)));
            let labels=serde_json::to_string(&issue.labels)?; let evidence=serde_json::to_string(&issue.evidence)?; let metadata=serde_json::to_string(&issue.metadata)?;
            tx.execute("UPDATE marbles_issue SET title=$3,description=$4,priority=$5,status=$6,closed_at=$7,labels_json=$8,parent=$9,available_at=$10,updated_at=$11,evidence_json=$12,metadata_json=$13,external_ref=$14 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&issue.title,&issue.description,&issue.priority,&issue.status,&issue.closed_at,&labels,&issue.parent,&available,&at,&evidence,&metadata,&issue.external_ref])?;
            if status_changed { log_event(tx,&self.company_id,id,at,actor,"status",&format!("→ {}",issue.status))?; } log_event(tx,&self.company_id,id,at,actor,"updated","")
        })?;
        self.get(id)
    }

    fn add_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx| { let row=tx.query_opt("SELECT labels_json FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&id])?.ok_or_else(||Error::NotFound(id.into()))?;let mut labels:Vec<String>=serde_json::from_str(row.get(0))?;if !labels.iter().any(|v|v==label){labels.push(label.into());labels.sort();let encoded=serde_json::to_string(&labels)?;tx.execute("UPDATE marbles_issue SET labels_json=$3,updated_at=$4 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&encoded,&at])?;log_event(tx,&self.company_id,id,at,actor,"labels",&labels.join(","))?;}Ok(()) })
    }
    fn remove_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx| { let row=tx.query_opt("SELECT labels_json FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&id])?.ok_or_else(||Error::NotFound(id.into()))?;let mut labels:Vec<String>=serde_json::from_str(row.get(0))?;labels.retain(|v|v!=label);let encoded=serde_json::to_string(&labels)?;tx.execute("UPDATE marbles_issue SET labels_json=$3,updated_at=$4 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&encoded,&at])?;log_event(tx,&self.company_id,id,at,actor,"labels",&labels.join(",")) })
    }

    fn add_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        if child == parent {
            return Err(Error::Cycle {
                child: child.into(),
                parent: parent.into(),
            });
        }
        self.transaction(|tx| { require_issue(tx,&self.company_id,child)?;require_issue(tx,&self.company_id,parent)?; if reachable(tx,&self.company_id,parent,child)?{return Err(Error::Cycle{child:child.into(),parent:parent.into()});} tx.execute("INSERT INTO marbles_dep(company_id,issue_id,depends_on) VALUES($1,$2,$3) ON CONFLICT DO NOTHING", &[&self.company_id,&child,&parent])?;tx.execute("UPDATE marbles_issue SET updated_at=$3 WHERE company_id=$1 AND id=$2", &[&self.company_id,&child,&at])?;log_event(tx,&self.company_id,child,at,actor,"depends_on",parent) })
    }
    fn remove_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx| {
            tx.execute(
                "DELETE FROM marbles_dep WHERE company_id=$1 AND issue_id=$2 AND depends_on=$3",
                &[&self.company_id, &child, &parent],
            )?;
            log_event(
                tx,
                &self.company_id,
                child,
                at,
                actor,
                "unblocked_from",
                parent,
            )
        })
    }

    fn claim(&self, req: &ClaimRequest, at: i64) -> Result<ClaimReceipt> {
        let ttl = match req.actor_kind {
            ActorKind::Agent => req
                .ttl_seconds
                .unwrap_or(self.policy.agent_ttl_seconds)
                .min(self.policy.max_ttl_seconds),
            ActorKind::Human => req
                .ttl_seconds
                .map(|s| {
                    self.week
                        .deadline(&utc(at), s.div_euclid(60).max(1))
                        .timestamp()
                        - at
                })
                .unwrap_or(self.week.human_expiry(&utc(at), &self.policy) - at)
                .min(self.policy.max_ttl_seconds),
        };
        self.transaction(|tx| { let row=tx.query_opt("SELECT assignee,expires_at FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&req.id])?.ok_or_else(||Error::NotFound(req.id.clone()))?;let assignee:Option<String>=row.get(0);let expires:Option<i64>=row.get(1);if let Some(holder)=&assignee && expires.is_some_and(|v|v>at) && holder!=&req.assignee {return Err(Error::AlreadyClaimed(req.id.clone(),holder.clone()));}let expiry=at+ttl;tx.execute("UPDATE marbles_issue SET assignee=$3,actor_kind=$4,claimed_at=$5,expires_at=$6,updated_at=$5 WHERE company_id=$1 AND id=$2", &[&self.company_id,&req.id,&req.assignee,&req.actor_kind.as_str(),&at,&expiry])?;log_event(tx,&self.company_id,&req.id,at,&req.assignee,"claimed",req.actor_kind.as_str())?;Ok(ClaimReceipt{id:req.id.clone(),assignee:req.assignee.clone(),actor_kind:req.actor_kind,expires_at:expiry}) })
    }
    fn touch(&self, id: &str, assignee: &str, at: i64) -> Result<i64> {
        self.transaction(|tx| { let row=tx.query_opt("SELECT assignee,actor_kind FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&id])?.ok_or_else(||Error::NotFound(id.into()))?;let current:Option<String>=row.get(0);if current.as_deref()!=Some(assignee){return Err(Error::Bad(format!("{id} is not claimed by {assignee}")));}let kind:Option<String>=row.get(1);let ttl=if kind.as_deref()==Some("human"){self.week.human_expiry(&utc(at),&self.policy)-at}else{self.policy.agent_ttl_seconds};let expiry=at+ttl;tx.execute("UPDATE marbles_issue SET expires_at=$3,updated_at=$4 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&expiry,&at])?;log_event(tx,&self.company_id,id,at,assignee,"heartbeat","")?;Ok(expiry) })
    }
    fn release(&self, id: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx|{let changed=tx.execute("UPDATE marbles_issue SET assignee=NULL,actor_kind=NULL,claimed_at=NULL,expires_at=NULL,updated_at=$3 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&at])?;if changed==0{return Err(Error::NotFound(id.into()));}log_event(tx,&self.company_id,id,at,actor,"released","")})
    }

    fn sweep(&self, at: i64) -> Result<SweepReport> {
        self.transaction(|tx| { let rows=tx.query("SELECT id,assignee,actor_kind FROM marbles_issue WHERE company_id=$1 AND assignee IS NOT NULL AND expires_at IS NOT NULL AND expires_at <= $2 FOR UPDATE", &[&self.company_id,&at])?;let mut report=SweepReport::default();for row in rows{let id:String=row.get(0);let assignee:String=row.get(1);let kind:String=row.get(2);if kind=="human"{report.escalated.push(format!("{id} ({assignee})"));log_event(tx,&self.company_id,&id,at,"sweeper","escalated",&assignee)?;}else{tx.execute("UPDATE marbles_issue SET assignee=NULL,actor_kind=NULL,claimed_at=NULL,expires_at=NULL WHERE company_id=$1 AND id=$2", &[&self.company_id,&id])?;log_event(tx,&self.company_id,&id,at,"sweeper","claim_expired",&assignee)?;report.requeued.push(id);}}Ok(report) })
    }

    fn close(
        &self,
        id: &str,
        reason: Option<&str>,
        evidence: &[Evidence],
        actor: &str,
        at: i64,
    ) -> Result<Issue> {
        if evidence.is_empty() {
            return Err(Error::NoEvidence(id.into()));
        }
        self.transaction(|tx|{let blockers=tx.query("SELECT d.depends_on FROM marbles_dep d JOIN marbles_issue p ON p.company_id=d.company_id AND p.id=d.depends_on WHERE d.company_id=$1 AND d.issue_id=$2 AND p.status!='closed' FOR UPDATE OF p", &[&self.company_id,&id])?;if let Some(row)=blockers.first(){let blocker:String=row.get(0);return Err(Error::Bad(format!("{id} still waits on {blocker}; close it first or remove the dependency")));}close_tx(tx,&self.company_id,id,reason,evidence,actor,at)})?;
        self.get(id)
    }
    fn supersede(&self, id: &str, replacement: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx|{require_issue(tx,&self.company_id,replacement)?;let reason=format!("superseded by {replacement}");close_tx(tx,&self.company_id,id,Some(&reason),&[Evidence::ack(reason.clone())],actor,at)?;let row=tx.query_one("SELECT labels_json FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&id])?;let mut labels:Vec<String>=serde_json::from_str(row.get(0))?;let label=format!("supersedes:{replacement}");if !labels.contains(&label){labels.push(label);labels.sort();let encoded=serde_json::to_string(&labels)?;tx.execute("UPDATE marbles_issue SET labels_json=$3,updated_at=$4 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&encoded,&at])?;log_event(tx,&self.company_id,id,at,actor,"labels",&labels.join(","))?;}Ok(())})
    }
    fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        self.with_client(|client| {
            Ok(client.query("SELECT issue_id,ts,actor,event,detail FROM marbles_history WHERE company_id=$1 AND issue_id=$2 ORDER BY seq", &[&self.company_id,&id])?.into_iter().map(|r|HistoryEvent{issue_id:r.get(0),ts:r.get(1),actor:r.get(2),event:r.get(3),detail:r.get(4)}).collect())
        })
    }
    fn stats(&self) -> Result<serde_json::Value> {
        self.with_client(|client| {
            let mut counts: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
            for r in client.query("SELECT project,status,COUNT(*) FROM marbles_issue WHERE company_id=$1 GROUP BY project,status", &[&self.company_id])?{counts.entry(r.get(0)).or_default().insert(r.get(1),r.get(2));}
            Ok(serde_json::json!(
                counts
                    .into_iter()
                    .map(|(project, statuses)| {
                        let total: i64 = statuses.values().sum();
                        (
                            project,
                            serde_json::json!({"by_status":statuses,"total":total}),
                        )
                    })
                    .collect::<serde_json::Map<_, _>>()
            ))
        })
    }
    fn prometheus_metrics(&self, at: i64) -> Result<String> {
        self.with_client(|client| {
        let mut out = String::from(
            "# HELP marbles_issues Current issues by project and status.\n# TYPE marbles_issues gauge\n# HELP marbles_queue_oldest_age_seconds Age of the oldest currently ready issue.\n# TYPE marbles_queue_oldest_age_seconds gauge\n# HELP marbles_claims_active Current unexpired claims by project and actor kind.\n# TYPE marbles_claims_active gauge\n# HELP marbles_events_total Durable tracker transitions by project and event.\n# TYPE marbles_events_total counter\n# HELP marbles_claim_latency_seconds Time from issue creation to first claim.\n# TYPE marbles_claim_latency_seconds summary\n# HELP marbles_cycle_time_seconds Time from issue creation to evidence-backed close.\n# TYPE marbles_cycle_time_seconds summary\n# HELP marbles_review_time_seconds Time from first review state to close.\n# TYPE marbles_review_time_seconds summary\n",
        );
        for r in client.query("SELECT project,status,COUNT(*) FROM marbles_issue WHERE company_id=$1 GROUP BY project,status ORDER BY project,status", &[&self.company_id])?{let p:String=r.get(0);let s:String=r.get(1);let n:i64=r.get(2);out.push_str(&format!("marbles_issues{{project=\"{}\",status=\"{}\"}} {n}\n",prometheus_label(&p),prometheus_label(&s)));}
        for r in client.query("SELECT i.project,$2-MIN(i.created_at) FROM marbles_issue i WHERE i.company_id=$1 AND i.status='open' AND COALESCE(i.available_at,0)<=$2 AND NOT EXISTS(SELECT 1 FROM marbles_dep d JOIN marbles_issue p ON p.company_id=d.company_id AND p.id=d.depends_on WHERE d.company_id=i.company_id AND d.issue_id=i.id AND p.status!='closed') GROUP BY i.project", &[&self.company_id,&at])?{let p:String=r.get(0);let age:i64=r.get(1);out.push_str(&format!("marbles_queue_oldest_age_seconds{{project=\"{}\"}} {}\n",prometheus_label(&p),age.max(0)));}
        for r in client.query("SELECT project,actor_kind,COUNT(*) FROM marbles_issue WHERE company_id=$1 AND assignee IS NOT NULL AND expires_at>$2 GROUP BY project,actor_kind", &[&self.company_id,&at])?{let p:String=r.get(0);let k:String=r.get(1);let n:i64=r.get(2);out.push_str(&format!("marbles_claims_active{{project=\"{}\",actor_kind=\"{}\"}} {n}\n",prometheus_label(&p),prometheus_label(&k)));}
        for r in client.query("SELECT i.project,h.event,COUNT(*) FROM marbles_history h JOIN marbles_issue i ON i.company_id=h.company_id AND i.id=h.issue_id WHERE h.company_id=$1 GROUP BY i.project,h.event", &[&self.company_id])?{let p:String=r.get(0);let e:String=r.get(1);let n:i64=r.get(2);out.push_str(&format!("marbles_events_total{{project=\"{}\",event=\"{}\"}} {n}\n",prometheus_label(&p),prometheus_label(&e)));}
        append_duration(
            &mut *client,
            &mut out,
            &self.company_id,
            "marbles_claim_latency_seconds",
            "SELECT i.project,SUM(h.first_claim-i.created_at)::bigint,COUNT(*) FROM marbles_issue i JOIN (SELECT company_id,issue_id,MIN(ts) first_claim FROM marbles_history WHERE company_id=$1 AND event='claimed' GROUP BY company_id,issue_id) h ON h.company_id=i.company_id AND h.issue_id=i.id WHERE i.company_id=$1 AND h.first_claim>=i.created_at GROUP BY i.project",
        )?;
        append_duration(
            &mut *client,
            &mut out,
            &self.company_id,
            "marbles_cycle_time_seconds",
            "SELECT project,SUM(closed_at-created_at)::bigint,COUNT(*) FROM marbles_issue WHERE company_id=$1 AND closed_at IS NOT NULL AND closed_at>=created_at GROUP BY project",
        )?;
        append_duration(
            &mut *client,
            &mut out,
            &self.company_id,
            "marbles_review_time_seconds",
            "SELECT i.project,SUM(i.closed_at-h.first_review)::bigint,COUNT(*) FROM marbles_issue i JOIN (SELECT company_id,issue_id,MIN(ts) first_review FROM marbles_history WHERE company_id=$1 AND event='status' AND detail='→ review' GROUP BY company_id,issue_id) h ON h.company_id=i.company_id AND h.issue_id=i.id WHERE i.company_id=$1 AND i.closed_at IS NOT NULL AND i.closed_at>=h.first_review GROUP BY i.project",
        )?;
        Ok(out)
        })
    }
}

fn row_to_issue(client: &mut impl GenericClient, company: &str, id: &str) -> Result<Option<Issue>> {
    let Some(r)=client.query_opt("SELECT id,title,description,status,issue_type,priority,parent,labels_json,assignee,actor_kind,expires_at,available_at,created_at,updated_at,project,closed_at,metadata_json,external_ref,evidence_json FROM marbles_issue WHERE company_id=$1 AND id=$2", &[&company,&id])? else{return Ok(None)};
    let deps=client.query("SELECT depends_on FROM marbles_dep WHERE company_id=$1 AND issue_id=$2 ORDER BY depends_on", &[&company,&id])?.into_iter().map(|r|Dependency{depends_on_id:Some(r.get(0))}).collect();
    let dependency_count:i64=client.query_one("SELECT COUNT(*) FROM marbles_dep d JOIN marbles_issue p ON p.company_id=d.company_id AND p.id=d.depends_on WHERE d.company_id=$1 AND d.issue_id=$2 AND p.status!='closed'", &[&company,&id])?.get(0);
    let dependent_count:i64=client.query_one("SELECT COUNT(*) FROM marbles_dep d JOIN marbles_issue c ON c.company_id=d.company_id AND c.id=d.issue_id WHERE d.company_id=$1 AND d.depends_on=$2 AND c.status!='closed'", &[&company,&id])?.get(0);
    Ok(Some(Issue {
        id: r.get(0),
        title: r.get(1),
        description: r.get(2),
        status: r.get(3),
        issue_type: r.get(4),
        priority: r.get(5),
        parent: r.get(6),
        labels: serde_json::from_str(r.get(7))?,
        assignee: r.get(8),
        actor_kind: r.get(9),
        expires_at: r.get(10),
        available_at: r.get(11),
        created_at: r.get(12),
        updated_at: r.get(13),
        project: r.get(14),
        closed_at: r.get(15),
        metadata: serde_json::from_str(r.get(16))?,
        external_ref: r.get(17),
        evidence: serde_json::from_str(r.get(18))?,
        dependencies: deps,
        dependency_count,
        dependent_count,
    }))
}
fn require_issue(client: &mut impl GenericClient, company: &str, id: &str) -> Result<()> {
    if client
        .query_opt(
            "SELECT 1 FROM marbles_issue WHERE company_id=$1 AND id=$2",
            &[&company, &id],
        )?
        .is_none()
    {
        Err(Error::NotFound(id.into()))
    } else {
        Ok(())
    }
}
fn log_event(
    tx: &mut Transaction<'_>,
    company: &str,
    id: &str,
    ts: i64,
    actor: &str,
    event: &str,
    detail: &str,
) -> Result<()> {
    let seq: i64 = tx
        .query_one(
            "SELECT COALESCE(MAX(seq),0)+1 FROM marbles_history WHERE company_id=$1",
            &[&company],
        )?
        .get(0);
    tx.execute("INSERT INTO marbles_history(company_id,seq,issue_id,ts,actor,event,detail) VALUES($1,$2,$3,$4,$5,$6,$7)", &[&company,&seq,&id,&ts,&actor,&event,&detail])?;
    tx.execute("INSERT INTO marbles_outbound_event(company_id,store_id,seq,issue_id,project,occurred_at,event,next_attempt_at) SELECT $1,$1,$2,id,project,$4,$5,$4 FROM marbles_issue WHERE company_id=$1 AND id=$3", &[&company,&seq,&id,&ts,&event])?;
    Ok(())
}
fn close_tx(
    tx: &mut Transaction<'_>,
    company: &str,
    id: &str,
    reason: Option<&str>,
    evidence: &[Evidence],
    actor: &str,
    at: i64,
) -> Result<()> {
    let row = tx
        .query_opt(
            "SELECT evidence_json FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE",
            &[&company, &id],
        )?
        .ok_or_else(|| Error::NotFound(id.into()))?;
    let mut merged: Vec<Evidence> = serde_json::from_str(row.get(0))?;
    for item in evidence {
        if !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    let encoded = serde_json::to_string(&merged)?;
    tx.execute("UPDATE marbles_issue SET status='closed',closed_at=$3,close_reason=$4,evidence_json=$5,assignee=NULL,actor_kind=NULL,claimed_at=NULL,expires_at=NULL,updated_at=$3 WHERE company_id=$1 AND id=$2", &[&company,&id,&at,&reason,&encoded])?;
    let detail = format!(
        "{} | {}",
        reason.unwrap_or(""),
        evidence
            .iter()
            .map(|e| format!("{}:{}", e.kind, e.value))
            .collect::<Vec<_>>()
            .join(" ")
    );
    log_event(tx, company, id, at, actor, "closed", &detail)
}
fn reachable(
    client: &mut impl GenericClient,
    company: &str,
    from: &str,
    target: &str,
) -> Result<bool> {
    Ok(client.query_opt("WITH RECURSIVE walk(id) AS (SELECT $2::text UNION SELECT d.depends_on FROM marbles_dep d JOIN walk w ON d.issue_id=w.id WHERE d.company_id=$1) SELECT 1 FROM walk WHERE id=$3 LIMIT 1", &[&company,&from,&target])?.is_some())
}
fn short_suffix() -> String {
    const A: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("rng");
    bytes
        .iter()
        .map(|b| A[*b as usize % A.len()] as char)
        .collect()
}
fn utc(secs: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).single().expect("valid epoch")
}
fn prometheus_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}
fn append_duration(
    client: &mut impl GenericClient,
    out: &mut String,
    company: &str,
    metric: &str,
    sql: &str,
) -> Result<()> {
    for row in client.query(sql, &[&company])? {
        let project: String = row.get(0);
        let sum: i64 = row.get(1);
        let count: i64 = row.get(2);
        let project = prometheus_label(&project);
        out.push_str(&format!(
            "{metric}_sum{{project=\"{project}\"}} {}\n",
            sum.max(0)
        ));
        out.push_str(&format!(
            "{metric}_count{{project=\"{project}\"}} {count}\n"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The reconnect decision logic is exercised without a database: `with_reconnect` is
    //! generic over the client and error types, so a fake client is enough to pin down
    //! "retry exactly once on a dropped connection, never on anything else" — and that a slow
    //! or stalled reconnect never blocks another caller on the slot mutex.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    #[derive(Debug, PartialEq, Eq)]
    enum FakeError {
        Dropped,
        Fatal,
        Reconnecting,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct FakeClient(u8);

    fn dropped(error: &FakeError) -> bool {
        *error == FakeError::Dropped
    }

    fn busy() -> FakeError {
        FakeError::Reconnecting
    }

    fn slot(client: Option<FakeClient>) -> Mutex<SlotState<FakeClient>> {
        Mutex::new(SlotState {
            client,
            reconnecting: false,
        })
    }

    fn stored(slot: &Mutex<SlotState<FakeClient>>) -> Option<u8> {
        slot.lock().unwrap().client.as_ref().map(|client| client.0)
    }

    #[test]
    fn sqlstate_classification_covers_only_connection_loss() {
        for code in [
            "08000", "08001", "08003", "08004", "08006", "57P01", "57P02", "57P03", "53300",
        ] {
            assert!(
                is_disconnect_sqlstate(Some(code)),
                "{code} should be a disconnect"
            );
        }
        for code in ["23505", "42601", "57014", "40001"] {
            assert!(
                !is_disconnect_sqlstate(Some(code)),
                "{code} is a statement error, not a disconnect"
            );
        }
        assert!(!is_disconnect_sqlstate(None));
    }

    #[test]
    fn socket_errors_are_connection_loss_but_other_io_is_not() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionAborted,
            ErrorKind::ConnectionReset,
            ErrorKind::NotConnected,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(is_disconnect_io_kind(kind), "{kind:?} is a dead socket");
        }
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidData,
            ErrorKind::WouldBlock,
        ] {
            assert!(
                !is_disconnect_io_kind(kind),
                "{kind:?} must not trigger a reconnect"
            );
        }
    }

    #[test]
    fn dropped_connection_is_replaced_and_retried_exactly_once() {
        let slot = slot(Some(FakeClient(1)));
        let mut connects = 0;
        let mut attempts = Vec::new();
        let value = with_reconnect(
            &slot,
            || {
                connects += 1;
                Ok(FakeClient(2))
            },
            |client| {
                attempts.push(client.0);
                if client.0 == 1 {
                    Err(FakeError::Dropped)
                } else {
                    Ok(client.0)
                }
            },
            dropped,
            Duration::ZERO,
            busy,
        );
        assert_eq!(value, Ok(2));
        assert_eq!(attempts, [1, 2], "the operation runs once per connection");
        assert_eq!(connects, 1);
        assert_eq!(stored(&slot), Some(2));
    }

    #[test]
    fn statement_errors_are_not_retried() {
        let slot = slot(Some(FakeClient(1)));
        let mut connects = 0;
        let error = with_reconnect(
            &slot,
            || {
                connects += 1;
                Ok(FakeClient(2))
            },
            |_| Err::<(), FakeError>(FakeError::Fatal),
            dropped,
            Duration::ZERO,
            busy,
        )
        .unwrap_err();
        assert_eq!(error, FakeError::Fatal);
        assert_eq!(connects, 0);
        assert_eq!(stored(&slot), Some(1));
    }

    #[test]
    fn failed_reconnect_surfaces_the_new_error_and_stays_retryable() {
        let slot = slot(Some(FakeClient(1)));
        let mut connects = 0;
        let error = with_reconnect(
            &slot,
            || {
                connects += 1;
                Err(FakeError::Fatal)
            },
            |_| Err::<(), FakeError>(FakeError::Dropped),
            dropped,
            Duration::ZERO,
            busy,
        )
        .unwrap_err();
        assert_eq!(
            error,
            FakeError::Fatal,
            "never the stale dropped-connection error"
        );
        assert_eq!(connects, 1);
        assert!(
            stored(&slot).is_none(),
            "a failed reconnect must not pin a dead client"
        );

        // The next operation dials again instead of staying dead forever.
        let value = with_reconnect(
            &slot,
            || Ok(FakeClient(3)),
            |client| Ok(client.0),
            dropped,
            Duration::ZERO,
            busy,
        );
        assert_eq!(value, Ok(3));
    }

    #[test]
    fn an_empty_slot_connects_before_the_first_operation() {
        let slot = slot(None);
        let value = with_reconnect(
            &slot,
            || Ok(FakeClient(7)),
            |client| Ok(client.0),
            dropped,
            Duration::ZERO,
            busy,
        );
        assert_eq!(value, Ok(7));
    }

    #[test]
    fn a_retry_that_drops_again_leaves_the_slot_empty_for_the_next_call() {
        let slot = slot(Some(FakeClient(1)));
        let error = with_reconnect(
            &slot,
            || Ok(FakeClient(2)),
            |_| Err::<(), FakeError>(FakeError::Dropped),
            dropped,
            Duration::ZERO,
            busy,
        )
        .unwrap_err();
        assert_eq!(error, FakeError::Dropped);
        assert!(
            stored(&slot).is_none(),
            "a replacement that also died must not be cached"
        );
    }

    #[test]
    fn a_retry_that_fails_for_another_reason_keeps_the_replacement() {
        let slot = slot(Some(FakeClient(1)));
        let mut attempts = 0;
        let error = with_reconnect(
            &slot,
            || Ok(FakeClient(2)),
            |client| {
                attempts += 1;
                if client.0 == 1 {
                    Err::<(), FakeError>(FakeError::Dropped)
                } else {
                    Err::<(), FakeError>(FakeError::Fatal)
                }
            },
            dropped,
            Duration::ZERO,
            busy,
        )
        .unwrap_err();
        assert_eq!(error, FakeError::Fatal);
        assert_eq!(attempts, 2);
        assert_eq!(
            stored(&slot),
            Some(2),
            "the healthy replacement stays cached for the next call"
        );
    }

    #[test]
    fn a_disconnect_after_commit_is_not_replayed() {
        // Mirrors `PostgresStore::transaction`: `commit_attempted` is set before COMMIT, and the
        // retry predicate refuses the ambiguous error so committed work cannot be duplicated.
        let slot = slot(Some(FakeClient(1)));
        let commit_attempted = std::cell::Cell::new(false);
        let mut attempts = 0;
        let mut connects = 0;
        let error = with_reconnect(
            &slot,
            || {
                connects += 1;
                Ok(FakeClient(2))
            },
            |_| {
                attempts += 1;
                commit_attempted.set(true);
                Err::<(), FakeError>(FakeError::Dropped)
            },
            |error| !commit_attempted.get() && dropped(error),
            Duration::ZERO,
            busy,
        )
        .unwrap_err();
        assert_eq!(error, FakeError::Dropped);
        assert_eq!(attempts, 1, "an ambiguous COMMIT must not be replayed");
        assert_eq!(connects, 0);
    }

    #[test]
    fn a_slow_reconnect_does_not_block_concurrent_callers() {
        let slot = slot(Some(FakeClient(1)));
        let dials = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let reconnecting = scope.spawn(|| {
                with_reconnect(
                    &slot,
                    || {
                        dials.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(400));
                        Ok(FakeClient(2))
                    },
                    |client| {
                        if client.0 == 1 {
                            Err(FakeError::Dropped)
                        } else {
                            Ok(())
                        }
                    },
                    dropped,
                    Duration::from_millis(1),
                    busy,
                )
            });

            // Wait until the first caller has released the lock to dial, then assert a second
            // caller gets a bounded error immediately instead of waiting for the dial.
            let wait = Instant::now();
            while !slot.lock().unwrap().reconnecting {
                assert!(
                    wait.elapsed() < Duration::from_secs(5),
                    "the reconnecting flag was never set"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            // `with_reconnect` sets the flag, releases the lock, then sleeps the retry pause
            // before it dials. Wait for the dial itself, or `dials == 1` below races that pause.
            while dials.load(Ordering::SeqCst) == 0 {
                assert!(
                    wait.elapsed() < Duration::from_secs(5),
                    "the reconnect never started dialing"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let caller_started = Instant::now();
            let got = with_reconnect(
                &slot,
                || -> std::result::Result<FakeClient, FakeError> {
                    panic!("a concurrent caller must not dial while a reconnect is in progress")
                },
                |_| Ok(()),
                dropped,
                Duration::ZERO,
                busy,
            );
            assert_eq!(
                got,
                Err(FakeError::Reconnecting),
                "the concurrent caller gets a bounded error, not a hang"
            );
            assert!(
                caller_started.elapsed() < Duration::from_millis(200),
                "the concurrent caller must not wait on the in-flight dial"
            );
            assert_eq!(dials.load(Ordering::SeqCst), 1);
            assert_eq!(reconnecting.join().unwrap(), Ok(()));
        });
        assert_eq!(stored(&slot), Some(2));
    }

    #[test]
    fn a_failed_reconnect_does_not_block_concurrent_callers() {
        let slot = slot(Some(FakeClient(1)));
        std::thread::scope(|scope| {
            let reconnecting = scope.spawn(|| {
                with_reconnect(
                    &slot,
                    || {
                        std::thread::sleep(Duration::from_millis(300));
                        Err::<FakeClient, _>(FakeError::Fatal)
                    },
                    |_| Err::<(), FakeError>(FakeError::Dropped),
                    dropped,
                    Duration::ZERO,
                    busy,
                )
            });
            let wait = Instant::now();
            while !slot.lock().unwrap().reconnecting {
                assert!(wait.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(5));
            }
            let started = Instant::now();
            let got = with_reconnect(
                &slot,
                || -> std::result::Result<FakeClient, FakeError> {
                    panic!("a concurrent caller must not dial while a reconnect is in progress")
                },
                |_| Ok(()),
                dropped,
                Duration::ZERO,
                busy,
            );
            assert_eq!(got, Err(FakeError::Reconnecting));
            assert!(started.elapsed() < Duration::from_millis(200));
            assert_eq!(reconnecting.join().unwrap(), Err(FakeError::Fatal));
        });
        assert!(
            stored(&slot).is_none(),
            "a failed reconnect must leave the slot empty for the next caller"
        );
    }
}
