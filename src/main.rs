//! `marbles` / `mb` — one binary, three modes:
//!
//! - **CLI (local)**: point at a SQLite file and act directly. One operator, one machine.
//! - **CLI (client)**: with `MARBLES_URL`/`--url`, every verb is a POST to the server.
//! - **Server**: `marbles serve` — the fleet mode. One process owns the database; agents
//!   heartbeat leases; the sweeper reclaims expired agent claims and escalates expired human
//!   holds every minute.
//!
//! The JSON output shapes match what a Beads-era client expects (`create` → `{"id": …}`,
//! `list`/`ready` → arrays of beads, `show` → a one-element array) so migration is a binary swap,
//! not a rewrite.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use marbles::assertion_cache::AssertionCache;
use marbles::auth::Auth;
use marbles::client::Client;
use marbles::config;
use marbles::db::{CompanyStoreRegistry, CompanyStores, Db, now};
use marbles::identity::{
    InitOptions, PinnedKey, RemoteRegistry, StoreIdentity, descriptor_from_arg,
    verify_descriptor_json,
};
use marbles::postgres_store::PostgresCompanyStores;
use marbles::refs::{CanonicalRef, remote_dependency_freshness};
use marbles::report;
use marbles::setup::Profile;
use marbles::types::*;

#[derive(Parser)]
#[command(
    name = "marbles",
    version,
    about = "a small issue tracker for fleets of coding agents"
)]
struct Cli {
    /// Project root to resolve (walks up to `.marbles/project.toml`).
    #[arg(short = 'C', long, default_value = ".")]
    dir: PathBuf,
    /// Server URL; when set, commands talk HTTP instead of the local database.
    #[arg(long, env = "MARBLES_URL", global = true)]
    url: Option<String>,
    /// Bearer token (static local token or OIDC JWT).
    #[arg(
        long,
        env = "MARBLES_TOKEN",
        global = true,
        hide_env_values = true,
        conflicts_with = "token_file"
    )]
    token: Option<String>,
    /// Private file containing a scoped bearer token; read afresh on every invocation.
    #[arg(long, env = "MARBLES_TOKEN_FILE", global = true)]
    token_file: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    /// Per-request HTTP deadline in milliseconds (connect is capped at 3s).
    #[arg(
        long,
        env = "MARBLES_TIMEOUT_MS",
        global = true,
        default_value_t = marbles::client::DEFAULT_TIMEOUT_MS,
        value_name = "MS"
    )]
    timeout_ms: u64,
    /// Override the acting identity in local mode.
    #[arg(long, global = true)]
    actor: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register this directory as a project (writes .marbles/project.toml).
    Init {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        quiet: bool,
        /// Do not install the managed Marbles block in AGENTS.md.
        #[arg(long)]
        no_hooks: bool,
        /// Skip automatic import when a legacy .beads store is present.
        #[arg(long)]
        no_migrate_beads: bool,
        /// Managed instruction profile installed by init.
        #[arg(long, default_value = "maintainer")]
        profile: String,
        /// Persist the Marbles service endpoint in project.toml.
        #[arg(long)]
        server_url: Option<String>,
        /// Put the project in this folder (created if it does not exist).
        #[arg(long)]
        folder: Option<String>,
    },
    /// Start the server (fleet mode).
    Serve {
        /// Override the listen address from server.toml.
        #[arg(long)]
        addr: Option<String>,
    },
    /// Mint a local static token; the filename *is* the identity.
    Login {
        #[arg(long, default_value = "owner")]
        name: String,
        /// OIDC issuer for remote login.
        #[arg(long)]
        issuer: Option<String>,
        /// OIDC client id for remote login.
        #[arg(long)]
        client_id: Option<String>,
        /// OIDC client secret for remote login.
        #[arg(long, env = "MARBLES_CLIENT_SECRET", hide_env_values = true)]
        client_secret: Option<String>,
        /// Local callback registered for the OIDC client.
        #[arg(long)]
        redirect_uri: Option<String>,
    },
    /// Mint a local agent token (for daemons/sandboxes).
    AgentToken {
        name: String,
    },
    /// Print the server URL + how to point at it.
    Url,
    /// Check the selected endpoint, or local store integrity when running locally.
    Doctor {
        /// Project to probe when talking to a hosted server.
        #[arg(long)]
        project: Option<String>,
    },
    Create(Box<CreateArgs>),
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        id: Option<String>,
        /// Accepted for Beads-compatibility; limits are applied after filtering.
        #[arg(long, default_value_t = 0)]
        limit: i64,
        #[arg(long)]
        project: Option<String>,
    },
    Ready {
        #[arg(long)]
        project: Option<String>,
    },
    Show {
        id: String,
    },
    Update(Box<UpdateArgs>),
    /// Move work to review: the PR is open, the merge is not. Not done yet — see README.
    Review {
        id: String,
        #[arg(long)]
        pr: Option<String>,
        #[arg(long)]
        commit: Option<String>,
    },
    Close {
        id: String,
        #[arg(long)]
        reason: Option<String>,
        /// The merged PR this issue delivered. Done means merged.
        #[arg(long)]
        pr: Option<String>,
        /// The merge commit sha.
        #[arg(long)]
        commit: Option<String>,
        /// Explicit "no delivery expected" for research/coordination outcomes.
        #[arg(long)]
        ack: Option<String>,
    },
    Supersede {
        id: String,
        #[arg(long)]
        with: String,
    },
    Dep {
        #[command(subcommand)]
        action: DepAction,
    },
    Label {
        #[command(subcommand)]
        action: LabelAction,
    },
    Claim {
        id: String,
        /// Who holds it (agents: session id; humans: ignored, identity is taken from the token).
        #[arg(long = "as")]
        as_: Option<String>,
        #[arg(long)]
        ttl: Option<i64>,
    },
    Touch {
        id: String,
        #[arg(long = "as")]
        as_: Option<String>,
    },
    Release {
        id: String,
        #[arg(long = "as")]
        as_: Option<String>,
    },
    /// Sweep expired claims now (human principals only over HTTP).
    Sweep,
    History {
        id: String,
    },
    Stats,
    Projects,
    /// Admin operations on a tracker store (local mode).
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Group projects into folders (personal, business-acme, …).
    Folder {
        #[command(subcommand)]
        action: FolderAction,
    },
    /// Goals are project-local roots for epics and tasks.
    Goal {
        #[command(subcommand)]
        action: GoalAction,
    },
    /// Epics belong to a goal.
    Epic {
        #[command(subcommand)]
        action: EpicAction,
    },
    /// Tasks belong to an epic (or any parent issue).
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Daily remaining-work series and burn rate for a project, goal, or epic.
    Burndown {
        #[arg(long)]
        project: Option<String>,
        /// Scope the report to this goal (and its descendants).
        #[arg(long)]
        goal: Option<String>,
        /// Scope the report to this epic (and its descendants).
        #[arg(long)]
        epic: Option<String>,
        /// Window in days.
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// Folder → project → goal rollup: ready work, WIP, review/blocked, staleness, burn rate.
    Portfolio {
        /// Only show projects in this folder slug.
        #[arg(long)]
        folder: Option<String>,
        /// Burn-rate window in days.
        #[arg(long, default_value_t = 14)]
        days: i64,
        /// Treat open/in-progress work untouched for this many days as stale.
        #[arg(long, default_value_t = 7)]
        stale_days: i64,
    },
    /// Write an atomic SQLite snapshot with `VACUUM INTO` (local mode only).
    Backup {
        /// Destination path; must not already exist.
        to: PathBuf,
    },
    /// Store identity: Ed25519 keypair, rotation, and the public `marbles.store/1` descriptor.
    Store {
        #[command(subcommand)]
        action: StoreAction,
    },
    /// Read-only federated remotes: pin another store's Ed25519 identity, verify its assertions
    /// offline, and rotate the pin with a named overlap window. Pins never grant writes.
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    /// The read-only Remotes workspace: each pinned store's identity and named freshness.
    ///
    /// Remote stores are not local work: they never appear in `mb projects`, `mb folder tree`,
    /// `mb portfolio`, or `mb ready`, and cannot be claimed, assigned, or closed locally.
    Remotes,
    /// Plans are DAGs over issues: inspect the graph, schedule it, take the frontier, or
    /// draft one from a spec before dispatching any worker.
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },
    /// Import a Beads export. JSONL (from `bd export`) or the `bd list --json` array shape.
    ///
    /// `--rewrite-prefix old:new` renames a whole store's ids — for the mistake where two
    /// independent trackers were configured with the same prefix (e.g. `pedalkernel-pro` sharing
    /// `pedalkernel`). Ids are otherwise kept verbatim. Re-running is safe: matching rows are
    /// `unchanged`, and a row that exists with different content aborts as a conflict without
    /// writing anything.
    ImportBd {
        file: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        root: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        /// Rename ids on the way in, e.g. `--rewrite-prefix pedalkernel:pedalkernel-pro`.
        #[arg(long, value_parser = parse_pair)]
        rewrite_prefix: Option<(String, String)>,
        /// Validate and report without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install or refresh the managed instruction block in AGENTS.md/CLAUDE.md.
    Setup {
        #[arg(long, default_value = "maintainer")]
        profile: String,
        #[arg(long, default_value = "AGENTS.md")]
        target: String,
        #[arg(long)]
        remove: bool,
    },
}

#[derive(clap::Args)]
struct CreateArgs {
    title: String,
    #[arg(
        short = 't',
        long = "issue-type",
        alias = "type",
        default_value = "task"
    )]
    issue_type: String,
    #[arg(short = 'p', long, default_value_t = 2)]
    priority: i64,
    /// Comma-separated labels.
    #[arg(short = 'l', long, default_value = "")]
    labels: String,
    #[arg(short = 'd', long)]
    description: Option<String>,
    #[arg(long)]
    parent: Option<String>,
    /// Stable id for idempotent creation (crash between write and receipt recovery).
    #[arg(long)]
    id: Option<String>,
    /// Epoch seconds to override the creation grace (0 = swarm-visible immediately).
    /// Machine-created pipeline work uses this; humans keep the default deferral.
    #[arg(long)]
    available_at: Option<i64>,
    #[arg(long)]
    project: Option<String>,
    /// JSON object (inline, @file, or - for stdin) merged into the issue metadata.
    #[arg(long)]
    metadata: Option<String>,
    /// Stable pointer to the external system this issue mirrors (gap findings, imports).
    #[arg(long)]
    external_ref: Option<String>,
    /// Plan membership: makes this issue a node of the named plan DAG.
    #[arg(long)]
    plan: Option<String>,
    /// The command that proves this node done (plan metadata `done_means`).
    #[arg(long = "done-means")]
    done_means: Option<String>,
    /// Checkout this node edits (plan metadata `repo`).
    #[arg(long)]
    repo: Option<String>,
    /// Nodes sharing a conflict group never run concurrently.
    #[arg(long = "conflict-group")]
    conflict_group: Option<String>,
    /// low | medium | high | critical (plan metadata `risk`).
    #[arg(long)]
    risk: Option<String>,
    /// The test that accepts the outcome (plan metadata `acceptance`).
    #[arg(long)]
    acceptance: Option<String>,
    /// Minutes of expected work, feeding `plan gantt`.
    #[arg(long)]
    estimate: Option<i64>,
}

#[derive(clap::Args)]
struct UpdateArgs {
    id: String,
    #[arg(long)]
    description: Option<String>,
    #[arg(long)]
    status: Option<String>,
    #[arg(long)]
    priority: Option<i64>,
    #[arg(long)]
    append_notes: Option<String>,
    #[arg(long)]
    title: Option<String>,
    /// Claim the issue to this actor (`update` assignment is a claim, not a metadata edit).
    /// Remote refs are rejected: remote work is read-only.
    #[arg(long)]
    assignee: Option<String>,
    /// JSON object merged into metadata (null values delete keys).
    #[arg(long)]
    metadata: Option<String>,
    /// key=value sugar for one metadata key.
    #[arg(long = "set-metadata")]
    set_metadata: Option<String>,
}

#[derive(Subcommand)]
enum ProjectAction {
    /// Delete a store and every issue/edge/history row that touched it.
    Rm { slug: String },
}

#[derive(Subcommand)]
enum StoreAction {
    /// Create or inspect this store's Ed25519 identity.
    Identity {
        #[command(subcommand)]
        action: StoreIdentityAction,
    },
    /// Print the public, signed `marbles.store/1` descriptor.
    Descriptor {
        /// Emit compact single-line JSON instead of pretty JSON.
        #[arg(long)]
        compact: bool,
    },
    /// Verify a descriptor offline from a pinned store_id + key_id + public key.
    Verify {
        /// Descriptor JSON file, or `-` for stdin.
        descriptor: PathBuf,
        #[arg(long)]
        store_id: String,
        #[arg(long)]
        key_id: String,
        /// base64url raw Ed25519 public key the verifier pinned.
        #[arg(long)]
        public_key: String,
        /// Trust the key only until this unix timestamp (exclusive).
        #[arg(long)]
        valid_until: Option<i64>,
    },
}

#[derive(Subcommand)]
enum StoreIdentityAction {
    /// Create the identity if absent. Re-running is idempotent.
    Init {
        /// Human-readable store name (defaults to $HOSTNAME).
        #[arg(long)]
        display_name: Option<String>,
        /// Public endpoint advertised in the descriptor.
        #[arg(long)]
        endpoint: Option<String>,
        /// Capability to advertise; repeat for several (defaults to issues, claims, plans).
        #[arg(long = "capability")]
        capabilities: Vec<String>,
    },
    /// Show ids and key history; never private key material.
    Show,
    /// Add a new active key. The old key keeps verifying until the overlap window ends.
    Rotate {
        /// Seconds during which the old key still verifies (default 24h).
        #[arg(
            long,
            default_value_t = marbles::identity::DEFAULT_OVERLAP_SECONDS,
            value_name = "SECONDS"
        )]
        overlap_seconds: i64,
    },
}

#[derive(Subcommand)]
enum RemoteAction {
    /// Verify a remote's `marbles.store/1` descriptor and pin its Ed25519 identity locally.
    Add {
        name: String,
        /// Where the remote lives; a locator only — trust comes from the pinned key.
        endpoint: String,
        /// Descriptor JSON: file path, `-` for stdin, or an http(s) URL.
        #[arg(long)]
        descriptor: String,
    },
    /// List pinned remotes with their store_id + key_id (never a bare URL as trust).
    List,
    /// Show one pinned remote's verified identity.
    Show { name: String },
    /// Show cached issue assertions for a pinned remote, each with its named freshness.
    Assertions { name: String },
    /// Remove the local pin only; nothing is sent to the remote and no remote row changes.
    Remove { name: String },
    /// Move the pin to a rotated key, keeping the old key valid until the overlap ends.
    Rotate {
        name: String,
        /// Descriptor JSON: file path, `-` for stdin, or an http(s) URL.
        #[arg(long)]
        descriptor: String,
        /// Confirm the new base64url raw Ed25519 public key out of band. Strongly recommended
        /// for a key change: without it the new key is accepted on first use, like `add`.
        #[arg(long)]
        public_key: Option<String>,
        /// Seconds during which the old key still verifies (default 24h).
        #[arg(
            long,
            default_value_t = marbles::identity::DEFAULT_OVERLAP_SECONDS,
            value_name = "SECONDS"
        )]
        overlap_seconds: i64,
    },
}

#[derive(Subcommand)]
enum FolderAction {
    /// Create or rename a folder.
    Create {
        name: String,
        /// Stable slug (defaults to a slugified name).
        #[arg(long)]
        slug: Option<String>,
        /// Parent folder slug for nesting.
        #[arg(long)]
        parent: Option<String>,
    },
    /// List folders and their projects.
    List,
    /// Render the folder tree.
    Tree,
    /// Put a project in a folder; use `none` to unfile it.
    Move { project: String, folder: String },
}

#[derive(clap::Args)]
struct WorkCreateArgs {
    title: String,
    #[arg(short = 'p', long, default_value_t = 2)]
    priority: i64,
    #[arg(short = 'd', long)]
    description: Option<String>,
    #[arg(short = 'l', long, default_value = "")]
    labels: String,
    #[arg(long)]
    project: Option<String>,
    /// Epoch seconds to override the creation grace (0 = visible immediately).
    #[arg(long)]
    available_at: Option<i64>,
}

#[derive(Subcommand)]
enum GoalAction {
    /// Set a goal, optionally with a description.
    Create {
        #[command(flatten)]
        args: WorkCreateArgs,
    },
    /// List goals with descendant rollups.
    List {
        #[arg(long)]
        project: Option<String>,
        /// Include closed goals.
        #[arg(long)]
        all: bool,
    },
    /// Show one goal and its epic/task tree.
    Show { id: String },
}

#[derive(Subcommand)]
enum EpicAction {
    /// Register an epic under a goal.
    Create {
        #[arg(long)]
        goal: String,
        #[command(flatten)]
        args: WorkCreateArgs,
    },
}

#[derive(Subcommand)]
enum TaskAction {
    /// Register a task under an epic.
    Create {
        #[arg(long)]
        epic: String,
        #[command(flatten)]
        args: WorkCreateArgs,
    },
}

#[derive(Subcommand)]
enum PlanAction {
    /// Print the plan DAG: nodes in topological order, metadata, and edges.
    Show { plan: String },
    /// Estimated start/finish per node, the critical path, and current vs peak concurrency.
    Gantt { plan: String },
    /// The parallel frontier the executor consumes: deps closed and conflict group free.
    Ready { plan: String },
    /// Draft a plan from a TOML/JSON spec; writes nodes+deps unless --dry-run.
    Create {
        /// Spec path, or `-` for stdin. TOML (`name`, `project`, `[[nodes]]`) or JSON.
        spec: String,
        /// Plan name override (defaults to the spec's `name`).
        #[arg(long)]
        plan: Option<String>,
        /// Project override (defaults to the spec's `project`, then project discovery).
        #[arg(long)]
        project: Option<String>,
        /// Print the reviewable draft without writing any issue.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum DepAction {
    Add { child: String, parent: String },
    Rm { child: String, parent: String },
}

#[derive(Subcommand)]
enum LabelAction {
    Add { id: String, label: String },
    Remove { id: String, label: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Command::Login {
        issuer,
        client_id,
        client_secret: Some(secret),
        redirect_uri,
        ..
    } = &cli.command
    {
        let url = cli.url.as_deref().unwrap_or("https://marbles.fpl.dev");
        return match marbles::oidc::login(
            url,
            issuer.as_deref(),
            client_id.as_deref(),
            secret,
            redirect_uri.as_deref(),
        )
        .await
        {
            Ok(_) => {
                println!("signed in to {url}");
                ExitCode::SUCCESS
            }
            Err(err) => fail(err),
        };
    }
    // Explicit automation credentials must not depend on a stale browser profile.
    let saved = match if cli.token.is_some() || cli.token_file.is_some() {
        Ok(None)
    } else {
        marbles::oidc::load()
    } {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let effective_url = cli
        .url
        .clone()
        .or_else(|| saved.as_ref().map(|value| value.url.clone()))
        .or_else(|| {
            if matches!(&cli.command, Command::Init { .. }) {
                return None;
            }
            let dir = std::env::current_dir().ok()?.join(&cli.dir);
            config::discover_project(&dir).and_then(|(_, project)| project.server_url)
        });
    if effective_url.is_some()
        && matches!(
            &cli.command,
            Command::Login { .. } | Command::AgentToken { .. }
        )
    {
        return fail(
            "local token minting cannot enroll a hosted identity; use MARBLES_CLIENT_SECRET with hosted login, or an operator-provisioned company-scoped service bearer",
        );
    }
    let mode = if let Some(url) = &effective_url {
        let token = if let Some(token) = cli.token.clone() {
            token
        } else if let Some(path) = &cli.token_file {
            match read_bearer_file(path) {
                Ok(token) => token,
                Err(err) => return fail(err),
            }
        } else if let Some(saved) = saved {
            if saved.url.trim_end_matches('/') != url.trim_end_matches('/') {
                return fail(
                    "saved Marbles credentials belong to a different server; sign in there or supply an explicit scoped bearer token",
                );
            }
            match marbles::oidc::valid_token_with_timeout(saved, cli.timeout_ms).await {
                Ok((_, token)) => token,
                Err(err) => return fail(err),
            }
        } else {
            String::new()
        };
        if token.is_empty() {
            return fail(
                "remote Marbles has no credential; use MARBLES_CLIENT_SECRET with `mb --url <url> login`, or MARBLES_TOKEN_FILE for a scoped service bearer",
            );
        }
        Mode::Http(Client::with_options(
            url,
            &token,
            marbles::client::Options {
                timeout: Some(std::time::Duration::from_millis(cli.timeout_ms)),
                // Remote reads keep a local copy so an outage degrades to the last known graph.
                // Local mode never reaches this branch and never writes a cache.
                cache: Some(marbles::cache::Cache::at(marbles::cache::default_root())),
            },
        ))
    } else {
        let db = match Db::open(config::db_path()) {
            Ok(db) => Arc::new(db),
            Err(err) => return fail(format!("opening {}: {err}", config::db_path().display())),
        };
        Mode::Local(db)
    };
    match run(&cli, &mode, effective_url.as_deref()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => fail(message),
    }
}

enum Mode {
    Local(Arc<Db>),
    Http(Client),
}

impl Mode {
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        body: &impl serde::Serialize,
        local: impl FnOnce(&Db) -> Result<T, String>,
    ) -> Result<T, String> {
        match self {
            Mode::Http(client) => client.call(op, body).await,
            Mode::Local(db) => local(db),
        }
    }
}

fn actor_default() -> String {
    std::env::var("USER").unwrap_or_else(|_| "owner".to_string())
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("marbles: {message}");
    ExitCode::FAILURE
}

/// Split a remote reference into `(store, issue)`.
///
/// Two forms are recognized for F1.2: the canonical `marbles://<store>/<...>` prefix that F2 will
/// own, and the shorthand `<store_or_pin>/<issue>`. Local issue ids never contain `/`, so this
/// cannot mistake ordinary local work for a remote ref.
fn remote_ref_parts(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix("marbles://").unwrap_or(id);
    if id.trim() != id || id.chars().any(char::is_whitespace) {
        return None;
    }
    let (store, issue) = rest.split_once('/')?;
    if store.is_empty() || issue.is_empty() {
        return None;
    }
    Some((store, issue))
}

/// Refuse to mutate a remote reference before any local database or HTTP call happens.
///
/// This is the boundary that keeps a remote issue from ever looking local: a remote ref is a named
/// error with the pinned store_id, never a silent no-op and never a local row insert. A ref whose
/// store is not pinned is still rejected (read-only) with an actionable hint, so a typo cannot
/// accidentally fall through to the local store.
fn reject_remote_mutation(id: &str, action: &str) -> Result<(), String> {
    let Some((store, issue)) = remote_ref_parts(id) else {
        return Ok(());
    };
    let path = config::remotes_path();
    let registry = RemoteRegistry::load(&path).map_err(|err| {
        format!("remote refs are read-only: {id}; cannot {action} a remote issue ({err})")
    })?;
    let store_id = registry
        .list()
        .iter()
        .find(|record| record.store_id == store || record.name == store)
        .map(|record| record.store_id.as_str())
        .unwrap_or(store);
    Err(format!(
        "remote refs are read-only: {store_id}/{issue}; cannot {action} a remote issue locally \
         (remote pins are for read-only aggregation; inspect them with `mb remotes`)"
    ))
}

/// The project a command runs against: explicit flag, else discovery from -C.
fn project_for(cli: &Cli) -> Result<String, String> {
    let dir = std::env::current_dir().unwrap_or_default().join(&cli.dir);
    let dir = dir.canonicalize().unwrap_or(dir);
    if let Some((_, file)) = config::discover_project(&dir) {
        return Ok(file.slug);
    }
    Err(format!(
        "no .marbles/project.toml under {}; run `marbles init` there first",
        dir.display()
    ))
}

fn resolve_project(cli: &Cli, explicit: Option<&str>) -> Result<String, String> {
    explicit
        .map(str::to_string)
        .or_else(|| project_for(cli).ok())
        .filter(|project| !project.is_empty())
        .ok_or_else(|| "no project: run `marbles init` or pass --project".to_string())
}

async fn create_issue(mode: &Mode, spec: &NewIssue) -> Result<String, String> {
    match mode {
        Mode::Local(db) => db.create(spec, now()).map_err(|e| e.to_string()),
        Mode::Http(client) => {
            let response: serde_json::Value = client.call("issues.create", spec).await?;
            response["id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("issues.create returned no id: {response}"))
        }
    }
}

fn project_folder_map(view: &FolderView) -> BTreeMap<&str, &str> {
    view.projects
        .iter()
        .map(|p| (p.project.as_str(), p.folder.as_str()))
        .collect()
}

fn project_infos(db: &Db) -> Result<Vec<report::ProjectInfo>, String> {
    let view = db.folder_view().map_err(|e| e.to_string())?;
    let rows = db.projects().map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|(slug, root, prefix)| report::ProjectInfo {
            folder: view
                .projects
                .iter()
                .find(|p| p.project == slug)
                .map(|p| p.folder.clone()),
            slug,
            root,
            prefix,
        })
        .collect())
}

fn print_portfolio(report: &report::Portfolio) {
    println!(
        "portfolio — {} projects, {} issues, {} ready, {} in progress, {} stale",
        report.totals.projects,
        report.totals.progress.total,
        report.totals.ready,
        report.totals.progress.in_progress,
        report.totals.stale
    );
    match report.totals.burn_per_day {
        Some(rate) => println!("burn {rate:.2}/day across all projects"),
        None => println!("burn unknown (no closes in the window)"),
    }
    for folder in &report.folders {
        let mut parts = Vec::new();
        let mut cursor = folder.parent.as_deref();
        let mut guard = 0;
        while let Some(slug) = cursor {
            guard += 1;
            if guard > 16 {
                break;
            }
            match report
                .folders
                .iter()
                .find(|candidate| candidate.slug.as_deref() == Some(slug))
            {
                Some(parent) => {
                    parts.push(parent.name.clone());
                    cursor = parent.parent.as_deref();
                }
                None => break,
            }
        }
        parts.reverse();
        parts.push(folder.name.clone());
        println!(
            "\n{}  total {}  ready {}  stale {}  burn {}",
            parts.join("/"),
            folder.progress.total,
            folder.ready,
            folder.stale,
            folder
                .burn_per_day
                .map(|rate| format!("{rate:.2}/day"))
                .unwrap_or_else(|| "unknown".into())
        );
        for project in &folder.projects {
            print_project_rollup(project);
        }
    }
    if !report.unfiled.is_empty() {
        println!("\n(unfiled)");
        for project in &report.unfiled {
            print_project_rollup(project);
        }
    }
}

fn print_project_rollup(project: &report::ProjectRollup) {
    let oldest = project
        .oldest_ready_age_secs
        .map(|age| format!(" oldest-ready {}d", age / 86_400))
        .unwrap_or_default();
    println!(
        "  {:<22} total {:>3}  open {:>3}  wip {:>3}  review {:>3}  blocked {:>3}  ready {:>3}  stale {:>3}{}",
        project.slug,
        project.progress.total,
        project.progress.open,
        project.progress.in_progress,
        project.progress.review,
        project.progress.blocked,
        project.ready,
        project.stale,
        oldest
    );
    for goal in &project.goals {
        println!(
            "      goal {} [{}] {:.1}% ({}/{})",
            goal.id,
            goal.status,
            goal.progress.percent_done,
            goal.progress.closed,
            goal.progress.total
        );
    }
}

fn print_folder_view(view: &FolderView, tree: bool) {
    let assignments = project_folder_map(view);
    if tree {
        let mut children: BTreeMap<Option<&str>, Vec<&Folder>> = BTreeMap::new();
        for folder in &view.folders {
            children
                .entry(folder.parent.as_deref())
                .or_default()
                .push(folder);
        }
        for folders in children.values_mut() {
            folders.sort_by(|a, b| a.name.cmp(&b.name));
        }
        fn walk(
            parent: Option<&str>,
            depth: usize,
            children: &BTreeMap<Option<&str>, Vec<&Folder>>,
            view: &FolderView,
            out: &mut Vec<String>,
        ) {
            let Some(folders) = children.get(&parent) else {
                return;
            };
            for folder in folders {
                let indent = "  ".repeat(depth);
                let projects: Vec<&str> = view
                    .projects
                    .iter()
                    .filter(|p| p.folder == folder.slug)
                    .map(|p| p.project.as_str())
                    .collect();
                let suffix = if projects.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", projects.join(", "))
                };
                out.push(format!("{indent}{} ({}){suffix}", folder.name, folder.slug));
                walk(Some(&folder.slug), depth + 1, children, view, out);
            }
        }
        let mut lines = Vec::new();
        walk(None, 0, &children, view, &mut lines);
        let unfiled: Vec<&str> = assignments
            .iter()
            .filter(|(_, folder)| !view.folders.iter().any(|f| f.slug == **folder))
            .map(|(project, _)| *project)
            .collect();
        if !lines.is_empty() {
            println!("{}", lines.join("\n"));
        }
        if !unfiled.is_empty() {
            println!("(unfiled) [{}]", unfiled.join(", "));
        }
        return;
    }
    if view.folders.is_empty() {
        println!("no folders");
    }
    for folder in &view.folders {
        let mut projects: Vec<&str> = view
            .projects
            .iter()
            .filter(|p| p.folder == folder.slug)
            .map(|p| p.project.as_str())
            .collect();
        projects.sort();
        println!(
            "{:<24} {:<28} {}",
            folder.slug,
            folder.name,
            if projects.is_empty() {
                "-".to_string()
            } else {
                projects.join(", ")
            }
        );
    }
    let mut unfiled: Vec<&str> = view
        .projects
        .iter()
        .filter(|p| !view.folders.iter().any(|f| f.slug == p.folder))
        .map(|p| p.project.as_str())
        .collect();
    unfiled.sort();
    if !unfiled.is_empty() {
        println!("{:<24} {:<28} {}", "(unfiled)", "", unfiled.join(", "));
    }
}

async fn run(cli: &Cli, mode: &Mode, effective_url: Option<&str>) -> Result<(), String> {
    let actor = cli.actor.clone().unwrap_or_else(actor_default);
    match &cli.command {
        Command::Init {
            prefix,
            slug,
            quiet,
            no_hooks,
            no_migrate_beads,
            profile,
            server_url,
            folder,
        } => {
            let dir = std::env::current_dir().unwrap_or_default().join(&cli.dir);
            let dir = dir.canonicalize().unwrap_or(dir);
            let name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(config::slugify)
                .unwrap_or_default();
            if name.is_empty() {
                return Err("cannot derive a project name from this directory".into());
            }
            let slug = slug.clone().unwrap_or(name.clone());
            let prefix = prefix.clone().unwrap_or(name);
            let marbles_dir = dir.join(".marbles");
            std::fs::create_dir_all(&marbles_dir).map_err(|e| e.to_string())?;
            std::fs::write(
                marbles_dir.join("project.toml"),
                format!(
                    "slug = \"{slug}\"\nprefix = \"{prefix}\"\n{}",
                    server_url
                        .as_ref()
                        .map(|url| format!("server_url = \"{url}\"\n"))
                        .unwrap_or_default()
                ),
            )
            .map_err(|e| e.to_string())?;
            match mode {
                Mode::Local(db) => db
                    .ensure_project(&slug, &dir.to_string_lossy(), &prefix)
                    .map_err(|e| e.to_string())?,
                Mode::Http(client) => {
                    let _: serde_json::Value = client
                        .call(
                            "projects.ensure",
                            &serde_json::json!({"slug": slug, "root": dir.to_string_lossy(), "prefix": prefix}),
                        )
                        .await?;
                }
            }
            if let Some(folder) = folder {
                let folder = Folder {
                    slug: config::slugify(folder),
                    name: folder.clone(),
                    parent: None,
                    created_at: now(),
                };
                let _: serde_json::Value = mode
                    .call("folders.create", &folder, |db| {
                        db.ensure_folder(&folder, now())
                            .map_err(|e| e.to_string())?;
                        Ok(serde_json::json!({"slug": folder.slug}))
                    })
                    .await?;
                let _: serde_json::Value = mode
                    .call(
                        "projects.move",
                        &serde_json::json!({"project": slug, "folder": folder.slug}),
                        |db| {
                            db.set_project_folder(&slug, Some(&folder.slug), now())
                                .map_err(|e| e.to_string())?;
                            Ok(serde_json::json!({"project": slug, "folder": folder.slug}))
                        },
                    )
                    .await?;
            }
            if !*no_migrate_beads && dir.join(".beads").is_dir() {
                let db = require_local(
                    mode,
                    "automatic Beads migration currently requires local Marbles mode; unset MARBLES_URL or run `bd export | mb import-bd - --project <slug>` against the server",
                )?;
                let output = std::process::Command::new("bd")
                    .arg("export")
                    .current_dir(&dir)
                    .output()
                    .map_err(|e| {
                        format!("legacy .beads store found but `bd export` could not run: {e}")
                    })?;
                if !output.status.success() {
                    return Err(format!(
                        "legacy .beads export failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                let export = String::from_utf8(output.stdout)
                    .map_err(|e| format!("bd export was not UTF-8: {e}"))?;
                let rows =
                    marbles::jsonl::parse_jsonl(&export, "bd export").map_err(|e| e.to_string())?;
                let report = marbles::jsonl::import(db, &slug, &rows, None, false)
                    .map_err(|e| e.to_string())?;
                std::fs::write(
                    marbles_dir.join("beads-migrated"),
                    format!(
                        "project = \"{slug}\"\nissues = {}\n",
                        report.imported + report.unchanged
                    ),
                )
                .map_err(|e| format!("writing Beads migration receipt: {e}"))?;
                if !*quiet {
                    println!(
                        "migrated Beads into {slug}: {} imported, {} unchanged, {} dependencies",
                        report.imported, report.unchanged, report.deps
                    );
                }
            }
            if !*no_hooks {
                let profile = Profile::parse(profile).ok_or_else(|| {
                    format!("unknown profile {profile:?} (try conservative or maintainer)")
                })?;
                let path = dir.join("AGENTS.md");
                let existing = std::fs::read_to_string(&path).unwrap_or_default();
                let next = marbles::setup::apply(&existing, profile, env!("CARGO_PKG_VERSION"));
                std::fs::write(&path, next).map_err(|e| format!("{}: {e}", path.display()))?;
                if !*quiet {
                    println!("installed Marbles instructions in {}", path.display());
                }
            }
            if !*quiet {
                println!(
                    "initialized project {slug} (prefix {prefix}) at {}",
                    dir.display()
                );
            }
            Ok(())
        }
        Command::Serve { addr } => {
            let cfg = config::server_config();
            let webhook = cfg.webhook.clone();
            let listen = addr.clone().unwrap_or(cfg.listen);
            let database_url = std::env::var("DATABASE_URL")
                .ok()
                .filter(|url| !url.trim().is_empty());
            // The API requires a workstation fallback store, but hosted OIDC requests always
            // resolve through `company_stores`. Keep that fallback ephemeral in PostgreSQL mode
            // so a production process never creates or mutates a shadow SQLite authority.
            let db: Arc<dyn marbles::db::Store> = if database_url.is_some() {
                Arc::new(Db::in_memory().map_err(|e| format!("opening fallback store: {e}"))?)
            } else {
                Arc::new(Db::open(config::db_path()).map_err(|e| format!("opening database: {e}"))?)
            };
            let company_stores: Option<Arc<dyn CompanyStoreRegistry>> = match database_url {
                Some(url) => Some(Arc::new(
                    tokio::task::spawn_blocking(move || PostgresCompanyStores::connect(&url))
                        .await
                        .map_err(|_| "PostgreSQL startup worker failed".to_string())?
                        .map_err(|e| format!("opening hosted PostgreSQL store: {e}"))?,
                )),
                _ => cfg
                    .company_store_root
                    .map(CompanyStores::new)
                    .transpose()
                    .map_err(|e| format!("opening company stores: {e}"))?
                    .map(|stores| Arc::new(stores) as Arc<dyn CompanyStoreRegistry>),
            };
            let auth = Arc::new(Auth::new(cfg.auth, config::token_dir()));
            let api = Arc::new(marbles::api::Api {
                db: Arc::clone(&db),
                company_stores: company_stores.clone(),
                auth,
            });
            if let Some(webhook) = webhook {
                let publisher = marbles::webhook::Publisher::new(
                    webhook,
                    Arc::clone(&db),
                    company_stores.clone(),
                )?;
                tokio::spawn(publisher.run());
            }
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .map_err(|e| format!("binding {listen}: {e}"))?;
            println!(
                "marbles serving on http://{listen} (db {}, hosted stores {}, tokens {})",
                config::db_path().display(),
                company_stores
                    .as_ref()
                    .map(|_| "enabled")
                    .unwrap_or("disabled"),
                config::token_dir().display()
            );
            let sweeper_db = Arc::clone(&db);
            let sweeper_companies = company_stores;
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    tick.tick().await;
                    let companies = sweeper_companies.clone();
                    let db = Arc::clone(&sweeper_db);
                    let result = tokio::task::spawn_blocking(move || match &companies {
                        Some(stores) => stores.sweep_open(now()).map(|reports| {
                            reports.into_iter().fold(
                                marbles::db::SweepReport::default(),
                                |mut all, (_, report)| {
                                    all.requeued.extend(report.requeued);
                                    all.escalated.extend(report.escalated);
                                    all
                                },
                            )
                        }),
                        None => db.sweep(now()),
                    })
                    .await;
                    let result = match result {
                        Ok(result) => result,
                        Err(_) => {
                            eprintln!("sweep storage worker failed");
                            continue;
                        }
                    };
                    match result {
                        Ok(report) => {
                            if !report.requeued.is_empty() || !report.escalated.is_empty() {
                                eprintln!(
                                    "sweep: requeued {:?}, escalated {:?}",
                                    report.requeued, report.escalated
                                );
                            }
                        }
                        Err(err) => eprintln!("sweep failed: {err}"),
                    }
                }
            });
            axum::serve(listener, api.router())
                .await
                .map_err(|e| format!("server: {e}"))
        }
        Command::Login { name, .. } => {
            let (path, secret) =
                marbles::auth::mint_token(&config::token_dir(), ActorKind::Human, name)
                    .map_err(|e| e.to_string())?;
            println!("token: {secret}");
            println!("identity: human/{name} (file {})", path.display());
            Ok(())
        }
        Command::AgentToken { name } => {
            let (path, secret) =
                marbles::auth::mint_token(&config::token_dir(), ActorKind::Agent, name)
                    .map_err(|e| e.to_string())?;
            println!("token: {secret}");
            println!("identity: agent/{name} (file {})", path.display());
            Ok(())
        }
        Command::Url => {
            println!("{}", reported_url(effective_url));
            Ok(())
        }
        Command::Doctor { project } => {
            let Mode::Http(client) = mode else {
                let db = require_local(
                    mode,
                    "doctor reads the server's database file; run it on the host that owns --url",
                )?;
                let report = db.doctor().map_err(|e| e.to_string())?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                    );
                } else {
                    println!("integrity:    {}", report.integrity);
                    println!("quick check:  {}", report.quick_check);
                    println!(
                        "journal:      {} / synchronous {}",
                        report.journal_mode, report.synchronous
                    );
                    println!(
                        "storage:      {} bytes DB + {} bytes WAL",
                        report.db_bytes, report.wal_bytes
                    );
                    println!(
                        "rows:         {} projects, {} issues, {} deps, {} history, {} outbox pending",
                        report.projects,
                        report.issues,
                        report.deps,
                        report.history,
                        report.outbox_pending
                    );
                    if !report.foreign_key_violations.is_empty() {
                        println!("foreign keys: {}", report.foreign_key_violations.join("; "));
                    }
                }
                return Ok(());
            };
            let project = project.clone().or_else(|| project_for(cli).ok());
            if !client.health().await {
                return Err("hosted Marbles health check failed; check routing and deployment before re-authenticating".into());
            }
            let rows: Vec<Issue> = client
                .call("issues.ready", &serde_json::json!({"project":project}))
                .await?;
            let report = serde_json::json!({"mode":"hosted", "endpoint":effective_url,
                "health":"ok", "authentication":"ok", "project":project, "ready_count":rows.len()});
            if cli.json {
                println!("{report}");
            } else {
                println!("Marbles hosted: health and authenticated access OK");
            }
            Ok(())
        }
        Command::Create(args) => {
            let project = args
                .project
                .clone()
                .or(project_for(cli).ok())
                .unwrap_or_default();
            if project.is_empty() {
                return Err("no project: run `marbles init` or pass --project".into());
            }
            let spec = NewIssue {
                title: args.title.clone(),
                description: read_text_arg(args.description.as_deref())?,
                issue_type: args.issue_type.clone(),
                priority: args.priority,
                labels: split_csv(&args.labels),
                parent: args.parent.clone(),
                id: args.id.clone(),
                project: Some(project),
                available_at: args.available_at,
                created_by: Some(actor.clone()),
                metadata: Some(plan_node_metadata(args)?),
                external_ref: args.external_ref.clone(),
            };
            let id: String = match mode {
                Mode::Local(db) => db.create(&spec, now()).map_err(|e| e.to_string())?,
                Mode::Http(client) => {
                    let r: serde_json::Value = client.call("issues.create", &spec).await?;
                    r["id"].as_str().unwrap_or_default().to_string()
                }
            };
            if cli.json {
                println!("{}", serde_json::json!({"id": id}));
            } else {
                println!("{id} created");
            }
            Ok(())
        }
        Command::List {
            all,
            id,
            limit,
            project,
        } => {
            let project = project.clone().or(project_for(cli).ok());
            let body = serde_json::json!({"project": project, "all": all, "id": id});
            let rows: Vec<Issue> = mode
                .call("issues.list", &body, |db| {
                    db.list(project.as_deref(), *all, id.as_deref())
                        .map_err(|e| e.to_string())
                })
                .await?;
            let capped = if *limit <= 0 {
                rows
            } else {
                rows.into_iter().take(*limit as usize).collect()
            };
            print_issues(cli, &capped);
            Ok(())
        }
        Command::Ready { project } => {
            let project = project.clone().or(project_for(cli).ok());
            let body = serde_json::json!({"project": project});
            let rows: Vec<Issue> = mode
                .call("issues.ready", &body, |db| {
                    db.ready(project.as_deref(), now())
                        .map_err(|e| e.to_string())
                })
                .await?;
            print_issues(cli, &rows);
            Ok(())
        }
        Command::Show { id } => {
            // A one-element array: mirrors `bd show --json`, which the migrating clients parse as
            // a list. Boring compatibility is worth more than purity here.
            let body = serde_json::json!({"id": id});
            let row: Issue = mode
                .call("issues.get", &body, |db| {
                    db.get(id).map_err(|e| e.to_string())
                })
                .await?;
            let mut remote_dependencies = match mode {
                Mode::Local(db) => db.remote_dependencies(id).map_err(|e| e.to_string())?,
                // The edge itself is local client state; read it from the local store when the
                // issue body came over HTTP as well.
                Mode::Http(_) => Db::open(config::db_path())
                    .ok()
                    .and_then(|db| db.remote_dependencies(id).ok())
                    .unwrap_or_default(),
            };
            annotate_remote_dependencies(&mut remote_dependencies)?;
            if cli.json {
                let mut value = serde_json::to_value(&row).map_err(|e| e.to_string())?;
                if !remote_dependencies.is_empty() {
                    value["remote_dependencies"] =
                        serde_json::to_value(&remote_dependencies).map_err(|e| e.to_string())?;
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&vec![value]).map_err(|e| e.to_string())?
                );
            } else {
                let issue = row;
                println!("{} — {} [{}]", issue.id, issue.title, issue.status);
                if let Some(a) = &issue.assignee {
                    println!(
                        "claimed by {a} ({})",
                        issue.actor_kind.as_deref().unwrap_or("?")
                    );
                }
                for dependency in &remote_dependencies {
                    println!(
                        "waits on {} (remote; {}; state: {}; observed_at: {})",
                        dependency.canonical_ref,
                        dependency.freshness.as_deref().unwrap_or("unverifiable"),
                        dependency.state,
                        dependency.observed_at,
                    );
                }
                println!("{}", issue.description.trim());
            }
            Ok(())
        }
        Command::Update(args) => {
            reject_remote_mutation(
                &args.id,
                if args.assignee.is_some() {
                    "assign"
                } else {
                    "update"
                },
            )?;
            let metadata = match (json_arg(args.metadata.as_deref())?, &args.set_metadata) {
                (base, Some(kv)) => {
                    let (key, value) = kv
                        .split_once('=')
                        .ok_or_else(|| format!("--set-metadata wants key=value, got {kv}"))?;
                    let mut base = base.unwrap_or_else(|| serde_json::json!({}));
                    base.as_object_mut()
                        .expect("json_arg built an object")
                        .insert(key.to_string(), serde_json::json!(value));
                    Some(base)
                }
                (base, None) => base,
            };
            let patch = IssuePatch {
                title: args.title.clone(),
                description: args
                    .description
                    .as_deref()
                    .map(|d| read_text_arg(Some(d)))
                    .transpose()?,
                status: args.status.clone(),
                priority: args.priority,
                append_notes: args.append_notes.clone(),
                available_at: None,
                evidence: None,
                metadata,
            };
            if let Some(assignee) = &args.assignee {
                let body = serde_json::json!({
                    "id": args.id.clone(),
                    "assignee": assignee.clone(),
                    "ttl_seconds": null,
                });
                let receipt: ClaimReceipt = mode
                    .call("claims.claim", &body, |db| {
                        let kind = if assignee == &actor_default() {
                            ActorKind::Human
                        } else {
                            ActorKind::Agent
                        };
                        db.claim(
                            &ClaimRequest {
                                id: args.id.clone(),
                                assignee: assignee.clone(),
                                actor_kind: kind,
                                ttl_seconds: None,
                            },
                            now(),
                        )
                        .map_err(|e| e.to_string())
                    })
                    .await?;
                if !cli.json {
                    println!(
                        "assigned {} to {} ({}) until {}",
                        receipt.id,
                        receipt.assignee,
                        receipt.actor_kind.as_str(),
                        receipt.expires_at
                    );
                }
            }
            let body = serde_json::json!({"id": args.id, "patch": patch});
            let issue: Issue = mode
                .call("issues.update", &body, |db| {
                    db.update(&args.id, &patch, &actor, now())
                        .map_err(|e| e.to_string())
                })
                .await?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string(&issue).map_err(|e| e.to_string())?
                );
            } else {
                println!("{} updated ({})", issue.id, issue.status);
            }
            Ok(())
        }
        Command::Review { id, pr, commit } => {
            reject_remote_mutation(id, "review")?;
            let body = serde_json::json!({"id": id});
            let existing: Issue = mode
                .call("issues.get", &body, |db| {
                    db.get(id).map_err(|e| e.to_string())
                })
                .await?;
            let mut evidence = existing.evidence.clone();
            if let Some(pr) = pr {
                evidence.retain(|e| e.kind != "pr");
                evidence.push(Evidence::pr(pr.clone()));
            }
            if let Some(commit) = commit {
                evidence.retain(|e| e.kind != "commit");
                evidence.push(Evidence::commit(commit.clone()));
            }
            let patch = IssuePatch {
                status: Some("review".into()),
                evidence: Some(evidence),
                ..Default::default()
            };
            let body = serde_json::json!({"id": id, "patch": patch});
            let issue: Issue = mode
                .call("issues.update", &body, |db| {
                    db.update(id, &patch, &actor, now())
                        .map_err(|e| e.to_string())
                })
                .await?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string(&issue).map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "{} -> review (waiting on CI and a human; closed requires merge evidence)",
                    issue.id
                );
            }
            Ok(())
        }
        Command::Close {
            id,
            reason,
            pr,
            commit,
            ack,
        } => {
            reject_remote_mutation(id, "close")?;
            let mut evidence: Vec<Evidence> = Vec::new();
            if let Some(pr) = pr {
                evidence.push(Evidence::pr(pr.clone()));
            }
            if let Some(commit) = commit {
                evidence.push(Evidence::commit(commit.clone()));
            }
            if let Some(ack) = ack {
                evidence.push(Evidence::ack(ack.clone()));
            }
            let body = serde_json::json!({"id": id, "reason": reason, "evidence": evidence});
            let issue: Issue = mode
                .call("issues.close", &body, |db| {
                    db.close(id, reason.as_deref(), &evidence, &actor, now())
                        .map_err(|e| e.to_string())
                })
                .await?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string(&issue).map_err(|e| e.to_string())?
                );
            } else {
                println!("{} closed", issue.id);
            }
            Ok(())
        }
        Command::Supersede { id, with } => {
            reject_remote_mutation(id, "supersede")?;
            reject_remote_mutation(with, "supersede")?;
            let body = serde_json::json!({"id": id, "replacement": with});
            let _: serde_json::Value = mode
                .call("issues.supersede", &body, |db| {
                    db.supersede(id, with, &actor, now())
                        .map(|_| serde_json::json!({"ok": true}))
                        .map_err(|e| e.to_string())
                })
                .await?;
            println!("{id} superseded by {with}");
            Ok(())
        }
        Command::Dep { action } => {
            let (child, parent, add) = match action {
                DepAction::Add { child, parent } => (child, parent, true),
                DepAction::Rm { child, parent } => (child, parent, false),
            };
            reject_remote_mutation(child, "add a dependency to")?;
            // A canonical parent is a remote edge, not a local dep. It is verified offline from
            // local pins/cache and stored on the child; the remote issue never becomes a row.
            if parent.starts_with("marbles://") {
                let reference = CanonicalRef::parse(parent)
                    .map_err(|error| format!("invalid canonical remote ref {parent:?}: {error}"))?;
                let Mode::Local(db) = mode else {
                    return Err(format!(
                        "remote dependencies are local client state; `mb dep` over HTTP cannot \
                         store the verified edge for {reference} (run it against the local store)"
                    ));
                };
                let at = now();
                if add {
                    let registry = RemoteRegistry::load(&config::remotes_path())
                        .map_err(|error| format!("remote registry rejected: {error}"))?;
                    let cache = AssertionCache::at(config::assertion_cache_dir());
                    let view = cache.read(
                        &registry,
                        &reference.store_id,
                        reference.remote_issue_id(),
                        at,
                    );
                    let Some(assertion) = view.assertion else {
                        return Err(format!(
                            "cannot add remote dependency {reference}: {} ({})",
                            view.freshness,
                            view.problem
                                .or_else(|| view.missing.map(|reason| format!("{reason:?}")))
                                .unwrap_or_else(|| "no verified assertion".to_string())
                        ));
                    };
                    let dependency = RemoteDependency {
                        canonical_ref: reference.to_string(),
                        store_id: assertion.store_id,
                        remote_issue_id: assertion.remote_issue_id,
                        state: assertion.state,
                        asserted_at: assertion.asserted_at,
                        observed_at: assertion.observed_at,
                        signature: assertion.signature,
                        key_id: assertion.key_id,
                        freshness: None,
                    };
                    db.add_remote_dep(child, &dependency, &actor, at)
                        .map_err(|error| error.to_string())?;
                    println!("ok");
                } else {
                    db.remove_remote_dep(child, &reference.to_string(), &actor, at)
                        .map_err(|error| error.to_string())?;
                    println!("ok");
                }
                return Ok(());
            }
            reject_remote_mutation(parent, "add a dependency to")?;
            let body = serde_json::json!({"child": child, "parent": parent});
            let _: serde_json::Value = mode
                .call(if add { "deps.add" } else { "deps.remove" }, &body, |db| {
                    if add {
                        db.add_dep(child, parent, &actor, now())
                    } else {
                        db.remove_dep(child, parent, &actor, now())
                    }
                    .map(|_| serde_json::json!({"ok": true}))
                    .map_err(|e| e.to_string())
                })
                .await?;
            println!("ok");
            Ok(())
        }
        Command::Label { action } => {
            let (id, label, add) = match action {
                LabelAction::Add { id, label } => (id, label, true),
                LabelAction::Remove { id, label } => (id, label, false),
            };
            reject_remote_mutation(id, "label")?;
            let body = serde_json::json!({"id": id, "label": label});
            let _: serde_json::Value = mode
                .call(
                    if add { "labels.add" } else { "labels.remove" },
                    &body,
                    |db| {
                        if add {
                            db.add_label(id, label, &actor, now())
                        } else {
                            db.remove_label(id, label, &actor, now())
                        }
                        .map(|_| serde_json::json!({"ok": true}))
                        .map_err(|e| e.to_string())
                    },
                )
                .await?;
            println!("ok");
            Ok(())
        }
        Command::Claim { id, as_, ttl } => {
            reject_remote_mutation(id, "claim")?;
            let assignee = as_.clone().unwrap_or_else(|| actor.clone());
            let body = serde_json::json!({"id": id, "assignee": assignee, "ttl_seconds": ttl});
            let receipt: ClaimReceipt = mode
                .call("claims.claim", &body, |db| {
                    let kind = if assignee == actor_default() {
                        ActorKind::Human
                    } else {
                        ActorKind::Agent
                    };
                    db.claim(
                        &ClaimRequest {
                            id: id.clone(),
                            assignee: assignee.clone(),
                            actor_kind: kind,
                            ttl_seconds: *ttl,
                        },
                        now(),
                    )
                    .map_err(|e| e.to_string())
                })
                .await?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string(&receipt).map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "claimed {} by {} ({}) until {}",
                    receipt.id,
                    receipt.assignee,
                    receipt.actor_kind.as_str(),
                    receipt.expires_at
                );
            }
            Ok(())
        }
        Command::Touch { id, as_ } => {
            reject_remote_mutation(id, "touch")?;
            let holder = as_.clone().unwrap_or_else(|| actor.clone());
            let body = serde_json::json!({"id": id, "assignee": holder});
            let _: serde_json::Value = mode
                .call("claims.touch", &body, |db| {
                    db.touch(id, &holder, now())
                        .map(|e| serde_json::json!({"expires_at": e}))
                        .map_err(|e| e.to_string())
                })
                .await?;
            println!("renewed {id}");
            Ok(())
        }
        Command::Release { id, as_ } => {
            reject_remote_mutation(id, "release")?;
            let holder = as_.clone().unwrap_or_else(|| actor.clone());
            let body = serde_json::json!({"id": id, "assignee": holder});
            let _: serde_json::Value = mode
                .call("claims.release", &body, |db| {
                    db.release(id, &holder, now())
                        .map(|_| serde_json::json!({"ok": true}))
                        .map_err(|e| e.to_string())
                })
                .await?;
            println!("released {id}");
            Ok(())
        }
        Command::Sweep => {
            let report: serde_json::Value = mode
                .call("claims.sweep", &serde_json::json!({}), |db| {
                    serde_json::to_value(db.sweep(now()).map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())
                })
                .await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Command::History { id } => {
            let body = serde_json::json!({"id": id});
            let events: Vec<HistoryEvent> = mode
                .call("history.get", &body, |db| {
                    db.history(id).map_err(|e| e.to_string())
                })
                .await?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string(&events).map_err(|e| e.to_string())?
                );
            } else {
                for event in events {
                    println!(
                        "{} {} {} {}",
                        event.ts, event.actor, event.event, event.detail
                    );
                }
            }
            Ok(())
        }
        Command::Stats => {
            let value: serde_json::Value = mode
                .call("stats", &serde_json::json!({}), |db| {
                    db.stats().map_err(|e| e.to_string())
                })
                .await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Command::Project { action } => match action {
            ProjectAction::Rm { slug } => {
                let db = require_local(
                    mode,
                    "project rm writes directly; run it where the database lives",
                )?;
                let report = db.drop_project(slug).map_err(|e| e.to_string())?;
                println!(
                    "{}",
                    serde_json::to_string(
                        &serde_json::json!({"removed": slug, "issues": report.issues})
                    )
                    .map_err(|e| e.to_string())?
                );
                Ok(())
            }
        },
        Command::Projects => {
            let value: serde_json::Value = mode
                .call("projects.list", &serde_json::json!({}), |db| {
                    let view = db.folder_view().map_err(|e| e.to_string())?;
                    Ok(serde_json::json!(
                        db.projects()
                            .map_err(|e| e.to_string())?
                            .into_iter()
                            .map(|(slug, root, prefix)| {
                                let folder = view
                                    .projects
                                    .iter()
                                    .find(|p| p.project == slug)
                                    .map(|p| p.folder.clone());
                                serde_json::json!({
                                    "slug": slug,
                                    "root": root,
                                    "prefix": prefix,
                                    "folder": folder,
                                })
                            })
                            .collect::<Vec<_>>()
                    ))
                })
                .await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Command::Folder { action } => {
            match action {
                FolderAction::Create { name, slug, parent } => {
                    let slug = slug.clone().unwrap_or_else(|| config::slugify(name));
                    if slug.is_empty() {
                        return Err("folder name produced an empty slug".into());
                    }
                    let folder = Folder {
                        slug: slug.clone(),
                        name: name.clone(),
                        parent: parent.clone(),
                        created_at: now(),
                    };
                    let _: serde_json::Value = mode
                        .call("folders.create", &folder, |db| {
                            db.ensure_folder(&folder, now())
                                .map_err(|e| e.to_string())?;
                            Ok(serde_json::json!({"slug": folder.slug}))
                        })
                        .await?;
                    if cli.json {
                        println!("{}", serde_json::json!({"slug": slug}));
                    } else {
                        println!("folder {slug} ready");
                    }
                }
                FolderAction::List | FolderAction::Tree => {
                    let view: FolderView = mode
                        .call("folders.list", &serde_json::json!({}), |db| {
                            db.folder_view().map_err(|e| e.to_string())
                        })
                        .await?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&view).map_err(|e| e.to_string())?
                        );
                    } else {
                        print_folder_view(&view, matches!(action, FolderAction::Tree));
                    }
                }
                FolderAction::Move { project, folder } => {
                    let target = if folder == "none" || folder == "-" || folder.is_empty() {
                        None
                    } else {
                        Some(folder.as_str())
                    };
                    let body = serde_json::json!({"project": project, "folder": target});
                    let _: serde_json::Value = mode
                        .call("projects.move", &body, |db| {
                            db.set_project_folder(project, target, now())
                                .map_err(|e| e.to_string())?;
                            Ok(serde_json::json!({"project": project, "folder": target}))
                        })
                        .await?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({"project": project, "folder": target})
                        );
                    } else {
                        match target {
                            Some(folder) => println!("{project} -> {folder}"),
                            None => println!("{project} unfiled"),
                        }
                    }
                }
            }
            Ok(())
        }
        Command::Goal { action } => {
            match action {
                GoalAction::Create { args } => {
                    let project = resolve_project(cli, args.project.as_deref())?;
                    let spec = NewIssue {
                        title: args.title.clone(),
                        description: read_text_arg(args.description.as_deref())?,
                        issue_type: "goal".into(),
                        priority: args.priority,
                        labels: split_csv(&args.labels),
                        parent: None,
                        id: None,
                        project: Some(project),
                        available_at: args.available_at,
                        created_by: Some(actor.clone()),
                        metadata: None,
                        external_ref: None,
                    };
                    let id = create_issue(mode, &spec).await?;
                    if cli.json {
                        println!("{}", serde_json::json!({"id": id}));
                    } else {
                        println!("{id} goal created");
                    }
                }
                GoalAction::List { project, all } => {
                    let project = project.clone().or(project_for(cli).ok());
                    let body = serde_json::json!({"project": project, "all": true});
                    let issues: Vec<Issue> = mode
                        .call("issues.list", &body, |db| {
                            db.list(project.as_deref(), true, None)
                                .map_err(|e| e.to_string())
                        })
                        .await?;
                    let mut goals = report::goal_summaries(&issues, project.as_deref());
                    if !*all {
                        goals.retain(|goal| goal.status != "closed");
                    }
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&goals).map_err(|e| e.to_string())?
                        );
                    } else if goals.is_empty() {
                        println!("no goals; create one with `mb goal create \"…\"`");
                    } else {
                        for goal in goals {
                            println!(
                                "{} [{}] p{} {} — {}/{} closed ({}%)",
                                goal.id,
                                goal.status,
                                goal.priority,
                                goal.title,
                                goal.progress.closed,
                                goal.progress.total,
                                goal.progress.percent_done
                            );
                            println!("  {}", report::progress_bar(&goal.progress, 24));
                        }
                    }
                }
                GoalAction::Show { id } => {
                    let root: Issue = mode
                        .call("issues.get", &serde_json::json!({"id": id}), |db| {
                            db.get(id).map_err(|e| e.to_string())
                        })
                        .await?;
                    let body = serde_json::json!({"project": root.project, "all": true});
                    let issues: Vec<Issue> = mode
                        .call("issues.list", &body, |db| {
                            db.list(Some(&root.project), true, None)
                                .map_err(|e| e.to_string())
                        })
                        .await?;
                    let progress = report::progress_for(&issues, &root.id);
                    let lines = report::tree_lines(&issues, &root.id);
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "issue": root,
                                "progress": progress,
                                "tree": lines,
                            }))
                            .map_err(|e| e.to_string())?
                        );
                    } else {
                        println!(
                            "{} — {} [{}] p{} ({})",
                            root.id, root.title, root.status, root.priority, root.issue_type
                        );
                        println!("{}", report::progress_bar(&progress, 28));
                        for line in lines {
                            println!("{line}");
                        }
                    }
                }
            }
            Ok(())
        }
        Command::Epic { action } => {
            match action {
                EpicAction::Create { goal, args } => {
                    let goal_issue: Issue = mode
                        .call("issues.get", &serde_json::json!({"id": goal}), |db| {
                            db.get(goal).map_err(|e| e.to_string())
                        })
                        .await?;
                    if goal_issue.issue_type != "goal" {
                        return Err(format!("{goal} is a {}, not a goal", goal_issue.issue_type));
                    }
                    if let Some(project) = args.project.as_deref()
                        && project != goal_issue.project
                    {
                        return Err(format!(
                            "epic project {project} does not match goal project {}",
                            goal_issue.project
                        ));
                    }
                    let spec = NewIssue {
                        title: args.title.clone(),
                        description: read_text_arg(args.description.as_deref())?,
                        issue_type: "epic".into(),
                        priority: args.priority,
                        labels: split_csv(&args.labels),
                        parent: Some(goal_issue.id),
                        id: None,
                        project: Some(goal_issue.project),
                        available_at: args.available_at,
                        created_by: Some(actor.clone()),
                        metadata: None,
                        external_ref: None,
                    };
                    let id = create_issue(mode, &spec).await?;
                    if cli.json {
                        println!("{}", serde_json::json!({"id": id}));
                    } else {
                        println!("{id} epic created");
                    }
                }
            }
            Ok(())
        }
        Command::Task { action } => {
            match action {
                TaskAction::Create { epic, args } => {
                    let epic_issue: Issue = mode
                        .call("issues.get", &serde_json::json!({"id": epic}), |db| {
                            db.get(epic).map_err(|e| e.to_string())
                        })
                        .await?;
                    if epic_issue.issue_type != "epic" {
                        return Err(format!(
                            "{epic} is a {}, not an epic",
                            epic_issue.issue_type
                        ));
                    }
                    if let Some(project) = args.project.as_deref()
                        && project != epic_issue.project
                    {
                        return Err(format!(
                            "task project {project} does not match epic project {}",
                            epic_issue.project
                        ));
                    }
                    let spec = NewIssue {
                        title: args.title.clone(),
                        description: read_text_arg(args.description.as_deref())?,
                        issue_type: "task".into(),
                        priority: args.priority,
                        labels: split_csv(&args.labels),
                        parent: Some(epic_issue.id),
                        id: None,
                        project: Some(epic_issue.project),
                        available_at: args.available_at,
                        created_by: Some(actor.clone()),
                        metadata: None,
                        external_ref: None,
                    };
                    let id = create_issue(mode, &spec).await?;
                    if cli.json {
                        println!("{}", serde_json::json!({"id": id}));
                    } else {
                        println!("{id} task created");
                    }
                }
            }
            Ok(())
        }
        Command::Burndown {
            project,
            goal,
            epic,
            days,
        } => {
            let root = goal.as_ref().or(epic.as_ref());
            let mut project = project.clone().or(project_for(cli).ok());
            let issues: Vec<Issue> = if let Some(root_id) = root {
                let root_issue: Issue = mode
                    .call("issues.get", &serde_json::json!({"id": root_id}), |db| {
                        db.get(root_id).map_err(|e| e.to_string())
                    })
                    .await?;
                project = Some(root_issue.project.clone());
                let body = serde_json::json!({"project": root_issue.project, "all": true});
                let all: Vec<Issue> = mode
                    .call("issues.list", &body, |db| {
                        db.list(Some(&root_issue.project), true, None)
                            .map_err(|e| e.to_string())
                    })
                    .await?;
                let ids = report::descendant_ids(&all, root_id);
                all.into_iter()
                    .filter(|i| i.id != *root_id && ids.contains(&i.id))
                    .collect()
            } else {
                let body = serde_json::json!({"project": project, "all": true});
                mode.call("issues.list", &body, |db| {
                    db.list(project.as_deref(), true, None)
                        .map_err(|e| e.to_string())
                })
                .await?
            };
            let report = report::burndown(&issues, *days, now());
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "burndown {} — scope {} done {} remaining {}",
                    project.as_deref().unwrap_or("all"),
                    report.scope,
                    report.done,
                    report.remaining
                );
                match report.burn_per_day {
                    Some(rate) => println!(
                        "burn {:.2}/day; projected {:.1} days to zero",
                        rate,
                        report.projected_days_to_zero.unwrap_or(0.0)
                    ),
                    None => println!("burn unknown (no closes in the window)"),
                }
                for point in &report.points {
                    println!(
                        "{}  scope {:>4}  done {:>4}  left {:>4}  ideal {:>4}",
                        point.date, point.scope, point.done, point.remaining, point.ideal_remaining
                    );
                }
            }
            Ok(())
        }
        Command::Portfolio {
            folder,
            days,
            stale_days,
        } => {
            let view: FolderView = mode
                .call("folders.list", &serde_json::json!({}), |db| {
                    db.folder_view().map_err(|e| e.to_string())
                })
                .await?;
            let mut projects: Vec<report::ProjectInfo> = mode
                .call("projects.list", &serde_json::json!({}), |db| {
                    project_infos(db)
                })
                .await?;
            if let Some(folder) = folder {
                projects.retain(|project| project.folder.as_deref() == Some(folder.as_str()));
            }
            let issues: Vec<Issue> = mode
                .call("issues.list", &serde_json::json!({"all": true}), |db| {
                    db.list(None, true, None).map_err(|e| e.to_string())
                })
                .await?;
            let report =
                report::portfolio(&projects, &view.folders, &issues, now(), *days, *stale_days);
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                );
            } else {
                print_portfolio(&report);
            }
            Ok(())
        }
        Command::Backup { to } => {
            let db = require_local(
                mode,
                "backup reads the server's database file; run it on the host that owns --url",
            )?;
            let report = db.backup(to).map_err(|e| e.to_string())?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "backup {} ({} bytes): {} projects, {} issues, {} deps, {} history entries",
                    report.path,
                    report.bytes,
                    report.projects,
                    report.issues,
                    report.deps,
                    report.history
                );
            }
            Ok(())
        }
        Command::Plan { action } => match action {
            PlanAction::Show { plan } => {
                let view: marbles::plan::PlanView = mode
                    .call("plans.show", &serde_json::json!({"plan": plan}), |db| {
                        marbles::plan::load(db, plan)
                            .and_then(|graph| graph.view())
                            .map_err(|e| e.to_string())
                    })
                    .await?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&view).map_err(|e| e.to_string())?
                    );
                } else {
                    print!("{}", marbles::plan::render_plan(&view));
                }
                Ok(())
            }
            PlanAction::Gantt { plan } => {
                let gantt: marbles::plan::Gantt = mode
                    .call("plans.gantt", &serde_json::json!({"plan": plan}), |db| {
                        let graph = marbles::plan::load(db, plan).map_err(|e| e.to_string())?;
                        marbles::plan::gantt(&graph, now()).map_err(|e| e.to_string())
                    })
                    .await?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&gantt).map_err(|e| e.to_string())?
                    );
                } else {
                    print!("{}", marbles::plan::render_gantt(&gantt));
                }
                Ok(())
            }
            PlanAction::Ready { plan } => {
                let frontier: marbles::plan::ReadyView = mode
                    .call("plans.ready", &serde_json::json!({"plan": plan}), |db| {
                        let graph = marbles::plan::load(db, plan).map_err(|e| e.to_string())?;
                        marbles::plan::ready(&graph, now()).map_err(|e| e.to_string())
                    })
                    .await?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&frontier).map_err(|e| e.to_string())?
                    );
                } else {
                    print!("{}", marbles::plan::render_ready(&frontier));
                }
                Ok(())
            }
            PlanAction::Create {
                spec,
                plan,
                project,
                dry_run,
            } => {
                let text = read_spec_arg(spec)?;
                let spec = marbles::plan::parse_spec(&text).map_err(|e| e.to_string())?;
                let project = project
                    .clone()
                    .or(spec.project.clone())
                    .or_else(|| project_for(cli).ok())
                    .unwrap_or_default();
                let plan_name = plan
                    .clone()
                    .or(spec.name.clone())
                    .unwrap_or_else(|| "plan".to_string());
                let resolved = marbles::plan::ResolvedPlan {
                    plan: plan_name,
                    project,
                    nodes: spec.nodes,
                };
                if *dry_run {
                    let report = marbles::plan::draft(&resolved).map_err(|p| p.join("; "))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                        );
                    } else {
                        print!("{}", marbles::plan::render_draft(&report));
                    }
                    return Ok(());
                }
                let report: marbles::plan::CreateReport = mode
                    .call("plans.create", &resolved, |db| {
                        marbles::plan::create(db, &resolved, now(), &actor)
                    })
                    .await?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                    );
                } else {
                    print!("{}", marbles::plan::render_create(&report));
                }
                Ok(())
            }
        },
        Command::ImportBd {
            file,
            project,
            root,
            prefix,
            rewrite_prefix,
            dry_run,
        } => {
            let jsonl = looks_like_jsonl(file)?;
            let report = if jsonl {
                let db = require_local(
                    mode,
                    "JSONL import (with --rewrite-prefix/--dry-run) writes directly; \
                     run it where the database lives, or convert to the array form for HTTP",
                )?;
                db.ensure_project(
                    project,
                    &root
                        .clone()
                        .unwrap_or_else(|| format!("unhosted/{project}")),
                    &prefix.clone().unwrap_or_else(|| project.clone()),
                )
                .map_err(|e| e.to_string())?;
                let rows = marbles::jsonl::read_jsonl(file).map_err(|e| e.to_string())?;
                let pair = rewrite_prefix
                    .as_ref()
                    .map(|(a, b)| (a.as_str(), b.as_str()));
                serde_json::to_value(
                    marbles::jsonl::import(db, project, &rows, pair, *dry_run)
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?
            } else {
                if *dry_run || rewrite_prefix.is_some() {
                    return Err(
                        "--dry-run/--rewrite-prefix apply to the JSONL import path only".into(),
                    );
                }
                if let Mode::Local(db) = &mode {
                    db.ensure_project(
                        project,
                        &root
                            .clone()
                            .unwrap_or_else(|| format!("unhosted/{project}")),
                        &prefix.clone().unwrap_or_else(|| project.clone()),
                    )
                    .map_err(|e| e.to_string())?;
                    // In server mode the project must already be registered
                    // (`marbles init` with MARBLES_URL); the API enforces it.
                }
                let rows = marbles::import::read_export(file)?;
                let body = serde_json::json!({"project": project, "rows": rows});
                let imported: marbles::import::Report = mode
                    .call("issues.import", &body, |db| {
                        marbles::import::import(db, project, &rows)
                    })
                    .await?;
                serde_json::to_value(imported).map_err(|e| e.to_string())?
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            if *dry_run {
                eprintln!("dry run: nothing written");
            }
            Ok(())
        }
        Command::Setup {
            profile,
            target,
            remove,
        } => {
            let profile = Profile::parse(profile).ok_or_else(|| {
                format!("unknown profile {profile:?} (try conservative or maintainer)")
            })?;
            let path = std::env::current_dir()
                .unwrap_or_default()
                .join(&cli.dir)
                .join(target);
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            let next = if *remove {
                marbles::setup::strip(&existing).unwrap_or_default()
            } else {
                marbles::setup::apply(&existing, profile, env!("CARGO_PKG_VERSION"))
            };
            std::fs::write(&path, next).map_err(|e| format!("{}: {e}", path.display()))?;
            println!(
                "{} {}",
                path.display(),
                if *remove { "cleaned" } else { "updated" }
            );
            Ok(())
        }
        Command::Store { action } => run_store(action, cli.json),
        Command::Remote { action } => run_remote(action, cli.json).await,
        Command::Remotes => run_remotes(cli.json),
    }
}

/// Store identity is local state: it never travels over the tracker API and must not use the
/// webhook HMAC secret. Private keys stay under `$MARBLES_HOME` and are never printed.
fn run_store(action: &StoreAction, json: bool) -> Result<(), String> {
    let path = config::identity_path();
    match action {
        StoreAction::Identity { action } => match action {
            StoreIdentityAction::Init {
                display_name,
                endpoint,
                capabilities,
            } => {
                let existed = path.exists();
                let mut opts = InitOptions::new(
                    display_name
                        .clone()
                        .or_else(|| std::env::var("HOSTNAME").ok())
                        .unwrap_or_else(|| "marbles".to_string()),
                );
                opts.endpoint = endpoint.clone();
                if !capabilities.is_empty() {
                    opts.capabilities = capabilities.clone();
                }
                let identity =
                    StoreIdentity::init_at(&path, &opts, now()).map_err(|err| err.to_string())?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "created": !existed,
                            "path": path,
                            "store_id": identity.store_id,
                            "key_id": identity.active_key_id,
                        }))
                        .map_err(|err| err.to_string())?
                    );
                } else if existed {
                    println!(
                        "store identity already initialized: {} ({})",
                        identity.store_id, identity.active_key_id
                    );
                } else {
                    println!(
                        "initialized store identity {} with key {}",
                        identity.store_id, identity.active_key_id
                    );
                    println!("identity file: {} (keep private)", path.display());
                }
                Ok(())
            }
            StoreIdentityAction::Show => {
                let identity = StoreIdentity::load(&path).map_err(|err| err.to_string())?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&identity.public_view())
                        .map_err(|err| err.to_string())?
                );
                Ok(())
            }
            StoreIdentityAction::Rotate { overlap_seconds } => {
                let mut identity = StoreIdentity::load(&path).map_err(|err| err.to_string())?;
                let record = identity
                    .rotate(now(), *overlap_seconds)
                    .map_err(|err| err.to_string())?;
                identity.save(&path).map_err(|err| err.to_string())?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "old_key_id": record.old_key_id,
                        "new_key_id": record.new_key_id,
                        "rotated_at": record.rotated_at,
                        "overlap_until": record.overlap_until,
                        "note": "old key verifies until overlap_until, then fails closed",
                    }))
                    .map_err(|err| err.to_string())?
                );
                Ok(())
            }
        },
        StoreAction::Descriptor { compact } => {
            let identity = StoreIdentity::load(&path).map_err(|err| err.to_string())?;
            let descriptor = identity.descriptor(now()).map_err(|err| err.to_string())?;
            let text = if *compact {
                serde_json::to_string(&descriptor).map_err(|err| err.to_string())?
            } else {
                descriptor.to_json_pretty().map_err(|err| err.to_string())?
            };
            println!("{text}");
            Ok(())
        }
        StoreAction::Verify {
            descriptor,
            store_id,
            key_id,
            public_key,
            valid_until,
        } => {
            let text = if descriptor == std::path::Path::new("-") {
                let mut buffer = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
                    .map_err(|err| format!("reading stdin: {err}"))?;
                buffer
            } else {
                std::fs::read_to_string(descriptor)
                    .map_err(|err| format!("reading {}: {err}", descriptor.display()))?
            };
            let pin = PinnedKey {
                store_id: store_id.clone(),
                key_id: key_id.clone(),
                public_key: public_key.clone(),
                valid_until: *valid_until,
            };
            match verify_descriptor_json(&text, &pin, now()) {
                Ok(verified) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "verified": true,
                            "store_id": verified.store_id,
                            "key_id": verified.key_id,
                            "display_name": verified.display_name,
                            "issued_at": verified.issued_at,
                        }))
                        .map_err(|err| err.to_string())?
                    );
                    Ok(())
                }
                Err(err) => Err(format!("descriptor rejected: {err}")),
            }
        }
    }
}

/// Fill each remote edge's named freshness by re-verifying its stored assertion against the
/// current pin. A missing/corrupt registry makes every remote edge `unverifiable`, never fresh.
fn annotate_remote_dependencies(dependencies: &mut [RemoteDependency]) -> Result<(), String> {
    if dependencies.is_empty() {
        return Ok(());
    }
    let registry = RemoteRegistry::load(&config::remotes_path()).unwrap_or_default();
    let window = AssertionCache::at(config::assertion_cache_dir()).freshness_window_seconds();
    let at = now();
    for dependency in dependencies {
        dependency.freshness = Some(
            remote_dependency_freshness(dependency, &registry, at, window)
                .as_str()
                .to_string(),
        );
    }
    Ok(())
}

/// Pinned remotes are local client state: `$MARBLES_HOME/remotes.json`, never the tracker API.
/// A pin proves *who* a remote is for read-only aggregation; it can never authorize a write, and
/// `remove`/`rotate` never mutate the remote itself.
async fn run_remote(action: &RemoteAction, json: bool) -> Result<(), String> {
    let path = config::remotes_path();
    match action {
        RemoteAction::Add {
            name,
            endpoint,
            descriptor,
        } => {
            let text = descriptor_from_arg(descriptor)
                .await
                .map_err(|err| format!("descriptor rejected: {err}"))?;
            let mut registry = RemoteRegistry::load(&path).map_err(|err| err.to_string())?;
            let record = registry
                .add(name, endpoint, &text, now())
                .map_err(|err| format!("descriptor rejected: {err}"))?
                .clone();
            registry.save(&path).map_err(|err| err.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "added": true,
                        "registry": path,
                        "remote": record,
                    }))
                    .map_err(|err| err.to_string())?
                );
            } else {
                println!("pinned remote {}: {}", record.name, record.display_name);
                println!("  store_id: {}", record.store_id);
                println!("  key_id: {}", record.key_id);
                println!("  endpoint: {} (locator only)", record.endpoint);
            }
            Ok(())
        }
        RemoteAction::List => {
            let registry = RemoteRegistry::load(&path).map_err(|err| err.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&registry.list()).map_err(|err| err.to_string())?
                );
            } else if registry.list().is_empty() {
                println!("no remotes pinned ({})", path.display());
            } else {
                for record in registry.list() {
                    println!(
                        "{}\t{}\t{}\t{}",
                        record.name, record.store_id, record.key_id, record.endpoint
                    );
                }
            }
            Ok(())
        }
        RemoteAction::Show { name } => {
            let registry = RemoteRegistry::load(&path).map_err(|err| err.to_string())?;
            let record = registry
                .get(name)
                .ok_or_else(|| format!("remote {name:?} is not registered"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(record).map_err(|err| err.to_string())?
                );
            } else {
                println!("remote {}", record.name);
                println!("  display_name: {}", record.display_name);
                println!("  store_id: {}", record.store_id);
                println!("  key_id: {}", record.key_id);
                println!("  public_key: {}", record.public_key);
                println!("  endpoint: {} (locator only)", record.endpoint);
                println!(
                    "  valid_until: {}",
                    record
                        .valid_until
                        .map(|at| at.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                for key in &record.overlap_keys {
                    println!(
                        "  overlap key {}: valid until {}",
                        key.key_id,
                        key.valid_until
                            .map(|at| at.to_string())
                            .unwrap_or_else(|| "none".to_string())
                    );
                }
                println!(
                    "  capabilities: {} (read-only; pins never grant writes)",
                    record.capabilities.join(", ")
                );
            }
            Ok(())
        }
        RemoteAction::Assertions { name } => {
            let registry = RemoteRegistry::load(&path).map_err(|err| err.to_string())?;
            let record = registry
                .get(name)
                .ok_or_else(|| format!("remote {name:?} is not registered"))?;
            let cache = AssertionCache::at(config::assertion_cache_dir());
            let views = cache.read_all(&registry, &record.store_id, now());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "remote": record.name,
                        "store_id": record.store_id,
                        "freshness_window_seconds": cache.freshness_window_seconds(),
                        "assertions": views,
                    }))
                    .map_err(|err| err.to_string())?
                );
            } else if views.is_empty() {
                println!(
                    "no cached assertions for {} ({})",
                    record.name, record.store_id
                );
            } else {
                for view in &views {
                    println!(
                        "{}\t{}\t{}\t{}",
                        view.remote_issue_id,
                        view.asserted_state().unwrap_or("-"),
                        view.freshness.as_str(),
                        view.observed_at()
                            .map(|at| at.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    );
                    if let Some(reason) = &view.problem {
                        println!("  {reason}");
                    }
                }
            }
            Ok(())
        }
        RemoteAction::Remove { name } => {
            let mut registry = RemoteRegistry::load(&path).map_err(|err| err.to_string())?;
            let removed = registry.remove(name).map_err(|err| err.to_string())?;
            registry.save(&path).map_err(|err| err.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "removed": removed.name,
                        "store_id": removed.store_id,
                        "remote_unchanged": true,
                    }))
                    .map_err(|err| err.to_string())?
                );
            } else {
                println!(
                    "removed local pin for {} ({}) — the remote store was not contacted",
                    removed.name, removed.store_id
                );
            }
            Ok(())
        }
        RemoteAction::Rotate {
            name,
            descriptor,
            public_key,
            overlap_seconds,
        } => {
            let text = descriptor_from_arg(descriptor)
                .await
                .map_err(|err| format!("descriptor rejected: {err}"))?;
            let mut registry = RemoteRegistry::load(&path).map_err(|err| err.to_string())?;
            let record = registry
                .rotate(name, &text, public_key.as_deref(), *overlap_seconds, now())
                .map_err(|err| format!("rotation rejected: {err}"))?;
            registry.save(&path).map_err(|err| err.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": name,
                        "old_key_id": record.old_key_id,
                        "new_key_id": record.new_key_id,
                        "rotated_at": record.rotated_at,
                        "overlap_until": record.overlap_until,
                        "note": "old key verifies until overlap_until, then fails closed",
                    }))
                    .map_err(|err| err.to_string())?
                );
            } else {
                println!(
                    "rotated remote {}: {} -> {}",
                    name, record.old_key_id, record.new_key_id
                );
                println!(
                    "  overlap until {} (old key verifies until then, then fails closed)",
                    record.overlap_until
                );
            }
            Ok(())
        }
    }
}

/// `mb remotes` is the Remotes workspace, not a project list.
///
/// It projects local client state only: the pinned registry plus the assertion cache. It never
/// opens the tracker API and never writes a local row, so a remote store can never leak into the
/// authoritative project/folder/issue tables. A missing registry is stated plainly and exits 0;
/// a corrupt registry fails closed with the parser's named error.
fn run_remotes(json: bool) -> Result<(), String> {
    let path = config::remotes_path();
    let registry =
        RemoteRegistry::load(&path).map_err(|err| format!("remote registry rejected: {err}"))?;
    let cache = AssertionCache::at(config::assertion_cache_dir());
    let workspace = report::remotes_workspace(&registry, &cache, &path, now());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&workspace).map_err(|err| err.to_string())?
        );
        return Ok(());
    }
    if workspace.remotes.is_empty() {
        if workspace.registry_present {
            println!("no remotes pinned ({})", path.display());
        } else {
            println!(
                "no remotes pinned yet: registry does not exist ({})",
                path.display()
            );
        }
        println!(
            "remote stores are a separate read-only workspace: they never appear in local \
             projects, folders, portfolio, or ready work"
        );
        return Ok(());
    }
    println!(
        "remotes — {} pinned, read-only; never local projects or ready work",
        workspace.remotes.len()
    );
    println!("name\tstore_id\tkey_id\tendpoint\tfreshness\tobserved_at\treferences");
    for entry in &workspace.remotes {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            entry.name,
            entry.store_id,
            entry.key_id,
            entry.endpoint,
            entry.freshness.as_str(),
            entry
                .observed_at
                .map(|at| at.to_string())
                .unwrap_or_else(|| "-".to_string()),
            entry.references,
        );
        if let Some(problem) = &entry.problem {
            println!("  {problem}");
        }
    }
    println!(
        "freshness window: {}s; per-issue detail: `mb remote assertions <name>`",
        workspace.freshness_window_seconds
    );
    Ok(())
}

/// The URL the current command is actually using. Remote client selection is resolved before
/// `run`; only genuine local mode falls back to the server's loopback listen address.
fn reported_url(effective_url: Option<&str>) -> String {
    effective_url
        .map(str::to_owned)
        .unwrap_or_else(|| config::server_config().listen)
}

/// Merge `mb create`'s plan-metadata sugar into one JSON object. Explicit `--metadata` keys win
/// only when the sugar is absent, so neither form silently shadows the other.
fn plan_node_metadata(args: &CreateArgs) -> Result<serde_json::Value, String> {
    let mut metadata = json_arg(args.metadata.as_deref())?.unwrap_or_else(|| serde_json::json!({}));
    if !metadata.is_object() {
        return Err("--metadata must be a JSON object".into());
    }
    let map = metadata
        .as_object_mut()
        .expect("checked metadata is an object");
    let sugar = [
        ("plan", args.plan.as_ref().map(|v| serde_json::json!(v))),
        (
            "done_means",
            args.done_means.as_ref().map(|v| serde_json::json!(v)),
        ),
        ("repo", args.repo.as_ref().map(|v| serde_json::json!(v))),
        (
            "conflict_group",
            args.conflict_group.as_ref().map(|v| serde_json::json!(v)),
        ),
        ("risk", args.risk.as_ref().map(|v| serde_json::json!(v))),
        (
            "acceptance",
            args.acceptance.as_ref().map(|v| serde_json::json!(v)),
        ),
        ("estimate", args.estimate.map(|v| serde_json::json!(v))),
    ];
    for (key, value) in sugar {
        if let Some(value) = value {
            map.insert(key.to_string(), value);
        }
    }
    Ok(metadata)
}

/// Plan specs come from a path, `-` (stdin), `@path`, or inline text — the same surfaces as
/// other text arguments, plus the file-path case a spec usually is.
fn read_spec_arg(value: &str) -> Result<String, String> {
    if value == "-" || value.starts_with('@') {
        return read_text_arg(Some(value));
    }
    match std::fs::read_to_string(value) {
        Ok(text) => Ok(text),
        Err(_) => Ok(value.to_string()),
    }
}

fn read_bearer_file(path: &std::path::Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("opening bearer file: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file
            .metadata()
            .map_err(|e| e.to_string())?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err("bearer file must be private (chmod 0600)".into());
        }
    }
    let mut value = String::new();
    std::io::Read::read_to_string(&mut std::io::Read::take(file, 16385), &mut value)
        .map_err(|e| format!("reading bearer file: {e}"))?;
    let token = value.trim();
    if token.is_empty() || value.len() > 16384 || token.chars().any(char::is_whitespace) {
        return Err("bearer file must contain one nonempty token of at most 16 KiB".into());
    }
    Ok(token.to_owned())
}

fn require_local<'a>(mode: &'a Mode, hint: &str) -> Result<&'a Arc<Db>, String> {
    match mode {
        Mode::Local(db) => Ok(db),
        Mode::Http(_) => Err(hint.to_string()),
    }
}

/// clap value parser for `old:new`.
fn parse_pair(text: &str) -> Result<(String, String), String> {
    let (from, to) = text
        .split_once(':')
        .ok_or_else(|| format!("--rewrite-prefix wants old:new, got {text:?}"))?;
    if from.is_empty() || to.is_empty() {
        return Err(format!(
            "--rewrite-prefix parts must be non-empty: {text:?}"
        ));
    }
    Ok((from.to_string(), to.to_string()))
}

/// JSONL if any line begins with `{` and carries `_type`; the array form begins with `[`.
fn looks_like_jsonl(path: &str) -> Result<bool, String> {
    let mut buf = [0u8; 8192];
    let mut file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let n = std::io::Read::read(&mut file, &mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    let first = head
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    Ok(first.starts_with('{') && first.contains("\"_type\""))
}

/// Parse an optional JSON argument passed inline, as @file, or as `-`.
fn json_arg(value: Option<&str>) -> Result<Option<serde_json::Value>, String> {
    match value {
        None => Ok(None),
        Some(text) => {
            let text = read_text_arg(Some(text))?;
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("metadata must be a JSON object: {e}"))?;
            if !value.is_object() {
                return Err("metadata must be a JSON object".into());
            }
            Ok(Some(value))
        }
    }
}

fn split_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// `-` reads stdin; `@path` reads a file; anything else is the literal text.
fn read_text_arg(value: Option<&str>) -> Result<String, String> {
    match value {
        None => Ok(String::new()),
        Some("-") => {
            use std::io::Read;
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| e.to_string())?;
            Ok(text)
        }
        Some(text) if text.starts_with('@') => {
            std::fs::read_to_string(&text[1..]).map_err(|e| format!("{}: {e}", &text[1..]))
        }
        Some(text) => Ok(text.to_string()),
    }
}

fn print_issues(cli: &Cli, rows: &[Issue]) {
    if cli.json {
        println!("{}", serde_json::to_string(rows).unwrap_or_default());
        return;
    }
    for row in rows {
        let claim = row
            .assignee
            .as_ref()
            .map(|a| {
                format!(
                    " [{a}{}] ",
                    row.actor_kind
                        .as_deref()
                        .map(|k| format!(":{k}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_else(|| " ".into());
        println!(
            "{}{}{} — {} — {} blocker(s)",
            row.id, claim, row.status, row.title, row.dependency_count
        );
    }
}

#[cfg(test)]
mod cli_tests {
    use super::reported_url;

    #[test]
    fn url_reports_the_selected_remote_instead_of_the_local_listen_default() {
        assert_eq!(
            reported_url(Some("https://marbles.fpl.dev")),
            "https://marbles.fpl.dev"
        );
    }
    #[test]
    fn private_bearer_file_is_read_fresh_and_rejects_empty_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "first\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(super::read_bearer_file(&path).unwrap(), "first");
        std::fs::write(&path, "rotated\n").unwrap();
        assert_eq!(super::read_bearer_file(&path).unwrap(), "rotated");
        std::fs::write(&path, " \n").unwrap();
        assert!(super::read_bearer_file(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bearer_file_rejects_publicly_readable_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "private-value").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = super::read_bearer_file(&path).unwrap_err();
        assert!(error.contains("0600"));
        assert!(!error.contains("private-value"));
    }
}
