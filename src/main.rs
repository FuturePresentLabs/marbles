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

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use marbles::auth::Auth;
use marbles::client::Client;
use marbles::config;
use marbles::db::{Db, now};
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
    #[arg(long, env = "MARBLES_TOKEN", global = true)]
    token: Option<String>,
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
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
    },
    /// Mint a local agent token (for daemons/sandboxes).
    AgentToken {
        name: String,
    },
    /// Print the server URL + how to point at it.
    Url,
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
    /// Import a Beads export: `bd list --all --json > beads.json && marbles import-bd beads.json`
    ImportBd {
        file: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        root: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
    },
    /// Install or refresh the managed instruction block in AGENTS.md/CLAUDE.md.
    Setup {
        #[arg(long, default_value = "conservative")]
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
    /// JSON object merged into metadata (null values delete keys).
    #[arg(long)]
    metadata: Option<String>,
    /// key=value sugar for one metadata key.
    #[arg(long = "set-metadata")]
    set_metadata: Option<String>,
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
    let mode = if let Some(url) = &cli.url {
        let token = cli.token.clone().unwrap_or_default();
        if token.is_empty() {
            return fail("MARBLES_URL is set but no token: pass --token or MARBLES_TOKEN");
        }
        Mode::Http(Client::new(url, &token))
    } else {
        let db = match Db::open(config::db_path()) {
            Ok(db) => Arc::new(db),
            Err(err) => return fail(format!("opening {}: {err}", config::db_path().display())),
        };
        Mode::Local(db)
    };
    match run(&cli, &mode).await {
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

async fn run(cli: &Cli, mode: &Mode) -> Result<(), String> {
    let actor = cli.actor.clone().unwrap_or_else(actor_default);
    match &cli.command {
        Command::Init {
            prefix,
            slug,
            quiet,
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
                format!("slug = \"{slug}\"\nprefix = \"{prefix}\"\n"),
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
            let listen = addr.clone().unwrap_or(cfg.listen);
            let db = Arc::new(
                Db::open(config::db_path()).map_err(|e| format!("opening database: {e}"))?,
            );
            let auth = Arc::new(Auth::new(cfg.auth, config::token_dir()));
            let api = Arc::new(marbles::api::Api {
                db: Arc::clone(&db),
                auth,
            });
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .map_err(|e| format!("binding {listen}: {e}"))?;
            println!(
                "marbles serving on http://{listen} (db {}, tokens {})",
                config::db_path().display(),
                config::token_dir().display()
            );
            let sweeper_db = Arc::clone(&db);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    tick.tick().await;
                    match sweeper_db.sweep(now()) {
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
        Command::Login { name } => {
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
            println!("{}", config::server_config().listen);
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
                metadata: json_arg(args.metadata.as_deref())?,
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
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&vec![row]).map_err(|e| e.to_string())?
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
                println!("{}", issue.description.trim());
            }
            Ok(())
        }
        Command::Update(args) => {
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
        Command::Projects => {
            let value: serde_json::Value = mode
                .call("projects.list", &serde_json::json!({}), |db| {
                    Ok(serde_json::json!(db.projects().map_err(|e| e.to_string())?
                        .into_iter()
                        .map(|(slug, root, prefix)| serde_json::json!({"slug": slug, "root": root, "prefix": prefix}))
                        .collect::<Vec<_>>()))
                })
                .await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Command::ImportBd {
            file,
            project,
            root,
            prefix,
        } => {
            let db = require_local(
                mode,
                "import-bd writes directly; run it where the database lives",
            )?;
            db.ensure_project(
                project,
                &root.clone().unwrap_or_else(|| ".".into()),
                &prefix.clone().unwrap_or_else(|| project.clone()),
            )
            .map_err(|e| e.to_string())?;
            let rows = marbles::import::read_export(file)?;
            let report = marbles::import::import(db, project, &rows)?;
            println!(
                "{}",
                serde_json::to_string(&report).map_err(|e| e.to_string())?
            );
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
    }
}

fn require_local<'a>(mode: &'a Mode, hint: &str) -> Result<&'a Arc<Db>, String> {
    match mode {
        Mode::Local(db) => Ok(db),
        Mode::Http(_) => Err(hint.to_string()),
    }
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
