//! Argument grammar for the records-backed `mb` CLI.
//!
//! The binary that runs on `soil:records` only builds for `wasm32-wasip2` with
//! the `soil` feature, so a test inside `bin/mb.rs` would only run on that
//! target. The grammar lives here instead: it compiles for the host, is covered
//! by `tests/records_cli.rs`, and `bin/mb.rs` is left with the translation from a
//! [`Request`] to `Store` calls.
//!
//! The surface deliberately matches the native CLI's JSON contract, including
//! the places where that contract is a Beads compatibility promise rather than a
//! pretty shape (`show` prints a one-element array). A caller should not be able
//! to tell which backend answered except by commands that genuinely need the
//! host's filesystem or a native store identity.

use std::collections::BTreeMap;

use crate::types::{ActorKind, Evidence};

/// Everything a caller resolved before the request proper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Scope filter. `None` is every project, which is what `Store::list(None, ..)`
    /// already means: a project scopes a query, it is not required identity.
    pub project: Option<String>,
    pub prefix: Option<String>,
    pub actor: String,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateArgs {
    pub title: String,
    pub description: String,
    pub priority: i64,
    pub issue_type: String,
    pub parent: Option<String>,
    pub labels: Vec<String>,
    /// Stable id for idempotent creation.
    pub id: Option<String>,
    /// Epoch seconds to override the creation grace.
    pub available_at: Option<i64>,
    /// Stable pointer to the external system this issue mirrors.
    pub external_ref: Option<String>,
    /// Raw `--metadata` JSON text; the runner parses it so the grammar stays free
    /// of JSON and a bad object is reported by the command that reads it.
    pub metadata: Option<String>,
    /// Plan-node sugar, the same names the native CLI uses.
    pub plan: Option<String>,
    pub done_means: Option<String>,
    pub repo: Option<String>,
    pub conflict_group: Option<String>,
    pub risk: Option<String>,
    pub acceptance: Option<String>,
    pub estimate: Option<i64>,
}

/// The shared shape of `goal create` / `epic create` / `task create`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCreateArgs {
    pub title: String,
    pub description: String,
    pub priority: i64,
    pub labels: Vec<String>,
    pub available_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateArgs {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i64>,
    pub append_notes: Option<String>,
    /// Assignment is a claim, not a metadata edit — the runner claims first.
    pub assignee: Option<String>,
    /// Raw JSON object, shallow-merged into the issue metadata.
    pub metadata: Option<String>,
    /// `key=value` sugar for one metadata key; `value` is stored as a JSON string.
    pub set_metadata: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderAction {
    Create {
        name: String,
        slug: Option<String>,
        parent: Option<String>,
    },
    List,
    Tree,
    Move {
        project: String,
        /// `None` unfiles the project.
        folder: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAction {
    Create(Box<WorkCreateArgs>),
    List { all: bool },
    Show { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpicAction {
    Create { goal: String, args: WorkCreateArgs },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskAction {
    Create { epic: String, args: WorkCreateArgs },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    Show {
        plan: String,
    },
    Gantt {
        plan: String,
    },
    Ready {
        plan: String,
    },
    Create {
        /// Inline TOML/JSON, or `-` for stdin. A path cannot be read: the host
        /// owns the filesystem this command is not given.
        spec: String,
        plan: Option<String>,
        project: Option<String>,
        dry_run: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    List {
        all: bool,
        id: Option<String>,
        limit: i64,
    },
    Ready,
    Show {
        id: String,
    },
    Create(Box<CreateArgs>),
    Update(Box<UpdateArgs>),
    Claim {
        id: String,
        kind: ActorKind,
        ttl_seconds: Option<i64>,
        /// `--as NAME`; defaults to the invocation actor.
        assignee: Option<String>,
    },
    Touch {
        id: String,
        holder: Option<String>,
    },
    Release {
        id: String,
        holder: Option<String>,
    },
    /// Move to review: the PR is open, the merge is not.
    Review {
        id: String,
        pr: Option<String>,
        commit: Option<String>,
    },
    Close {
        id: String,
        reason: Option<String>,
        evidence: Vec<Evidence>,
    },
    Supersede {
        id: String,
        with: String,
    },
    Dep {
        add: bool,
        child: String,
        parent: String,
    },
    Label {
        add: bool,
        id: String,
        label: String,
    },
    History {
        id: String,
    },
    Sweep,
    Stats,
    Projects,
    ProjectRemove {
        slug: String,
    },
    Folder(FolderAction),
    Goal(GoalAction),
    Epic(EpicAction),
    Task(TaskAction),
    Burndown {
        goal: Option<String>,
        epic: Option<String>,
        days: i64,
    },
    Portfolio {
        folder: Option<String>,
        days: i64,
        stale_days: i64,
    },
    Plan(PlanAction),
    Doctor,
}

impl Request {
    /// Whether this command can mutate the store. Used to avoid paying the
    /// outbox drain on a pure query; a conservative `false` only delays a
    /// retry until the next write.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Request::List { .. }
                | Request::Ready
                | Request::Show { .. }
                | Request::History { .. }
                | Request::Stats
                | Request::Projects
                | Request::Burndown { .. }
                | Request::Portfolio { .. }
                | Request::Doctor
        )
    }
}

/// Commands this backend answers. Named so the failure for anything else can
/// say what does work instead of echoing an unknown-word error.
pub const COMMANDS: &[&str] = &[
    "burndown",
    "claim",
    "close",
    "create",
    "dep",
    "doctor",
    "epic",
    "folder",
    "goal",
    "history",
    "label",
    "list",
    "plan",
    "portfolio",
    "project",
    "projects",
    "ready",
    "release",
    "review",
    "show",
    "stats",
    "supersede",
    "sweep",
    "task",
    "touch",
    "update",
];

/// Commands that need something a Spore does not have: the caller's filesystem,
/// a native Ed25519 store identity, or the host's own records book. Rejecting
/// them by name — and saying which need what — beats "unknown command".
pub const NATIVE_ONLY: &[(&str, &str)] = &[
    (
        "init",
        "writes .marbles/project.toml and the AGENTS.md block, which belong to the host's filesystem",
    ),
    (
        "setup",
        "rewrites AGENTS.md, which belongs to the host's filesystem",
    ),
    (
        "import-bd",
        "reads a Beads export from the host's filesystem; `mb create` and `plan create` are the records-side path",
    ),
    (
        "backup",
        "snapshots the host's records book; the book is the host's, not the Spore's",
    ),
    (
        "store",
        "signs a native Marbles store identity (Ed25519); a records book has no such identity",
    ),
    (
        "remote",
        "pins native remote identities and an on-disk assertion cache",
    ),
    (
        "remotes",
        "reads the native remote registry and assertion cache",
    ),
];

/// Flags that belong to the server/HTTP half of Marbles and have no meaning
/// against a records store the host already selected. Rejecting them by name
/// beats accepting them and quietly ignoring where the caller pointed.
const TRANSPORT_FLAGS: &[&str] = &["--url", "--token", "--token-file", "--timeout-ms"];

pub fn parse(argv: &[String], env: &BTreeMap<String, String>) -> Result<Invocation, String> {
    // `--json` is `global = true` in the native CLI, so `mb ready --json` is a
    // documented spelling and cannot be a leading-only flag. Every answer here
    // is already JSON, so the flag needs no state beyond being accepted.
    let argv = normalize(argv)
        .into_iter()
        .filter(|arg| arg.as_str() != "--json")
        .collect::<Vec<_>>();
    let mut project = lookup(env, "MARBLES_PROJECT");
    let mut prefix = lookup(env, "MARBLES_PREFIX");
    let mut actor = lookup(env, "MARBLES_ACTOR").unwrap_or_else(|| "agent".into());

    // Globals may appear before the command; the per-command parsers below read
    // their own flags.
    let mut index = 0;
    while let Some(arg) = argv.get(index) {
        match arg.as_str() {
            "--project" => {
                project = Some(value(&argv, index, "--project")?);
                index += 2;
            }
            "--prefix" => {
                prefix = Some(value(&argv, index, "--prefix")?);
                index += 2;
            }
            "--actor" => {
                actor = value(&argv, index, "--actor")?;
                index += 2;
            }
            flag if TRANSPORT_FLAGS.contains(&flag) => {
                return Err(format!(
                    "{flag} selects an HTTP endpoint; this build talks to the soil:records \
                     store the host opened. Use the native `marbles` binary for a server."
                ));
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option {flag}"));
            }
            _ => break,
        }
    }

    let rest = argv.get(index..).unwrap_or_default();
    let Some(name) = rest.first() else {
        return Err(usage());
    };
    let request = parse_command(name, &rest[1..])?;
    Ok(Invocation {
        project,
        prefix,
        actor,
        request,
    })
}

/// Split `--flag=value` into two arguments. clap accepts it in the native CLI,
/// so a script written against `marbles` should not fail here for the spelling.
fn normalize(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    for arg in argv {
        if arg.starts_with("--")
            && arg.len() > 2
            && let Some((flag, value)) = arg.split_once('=')
        {
            out.push(flag.to_string());
            out.push(value.to_string());
        } else {
            out.push(arg.clone());
        }
    }
    out
}

fn parse_command(name: &str, rest: &[String]) -> Result<Request, String> {
    match name {
        "list" => {
            let mut all = false;
            let mut id = None;
            let mut limit = 0;
            let mut index = 0;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--all" => {
                        all = true;
                        index += 1;
                    }
                    "--id" => {
                        id = Some(value(rest, index, "--id")?);
                        index += 2;
                    }
                    "--limit" => {
                        limit = value(rest, index, "--limit")?
                            .parse()
                            .map_err(|_| "--limit needs a number".to_string())?;
                        index += 2;
                    }
                    other => return Err(format!("unknown list option {other}")),
                }
            }
            Ok(Request::List { all, id, limit })
        }
        "ready" => no_flags(rest, Request::Ready),
        "show" => Ok(Request::Show {
            id: positional(rest, 0, "show ID")?,
        }),
        "history" => Ok(Request::History {
            id: positional(rest, 0, "history ID")?,
        }),
        "create" => Ok(Request::Create(Box::new(parse_create(rest)?))),
        "update" => Ok(Request::Update(Box::new(parse_update(rest)?))),
        "claim" => {
            let id = positional(rest, 0, "claim ID")?;
            let mut kind = ActorKind::Agent;
            let mut ttl_seconds = None;
            let mut assignee = None;
            let mut index = 1;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--human" => {
                        kind = ActorKind::Human;
                        index += 1;
                    }
                    "--ttl" => {
                        ttl_seconds = Some(
                            value(rest, index, "--ttl")?
                                .parse()
                                .map_err(|_| "--ttl needs seconds".to_string())?,
                        );
                        index += 2;
                    }
                    "--as" => {
                        assignee = Some(value(rest, index, "--as")?);
                        index += 2;
                    }
                    other => return Err(format!("unknown claim option {other}")),
                }
            }
            Ok(Request::Claim {
                id,
                kind,
                ttl_seconds,
                assignee,
            })
        }
        "touch" | "release" => {
            let id = positional(rest, 0, &format!("{name} ID"))?;
            let holder = holder_flag(rest, name)?;
            Ok(if name == "touch" {
                Request::Touch { id, holder }
            } else {
                Request::Release { id, holder }
            })
        }
        "review" => {
            let id = positional(rest, 0, "review ID")?;
            let mut pr = None;
            let mut commit = None;
            let mut index = 1;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--pr" => {
                        pr = Some(value(rest, index, "--pr")?);
                        index += 2;
                    }
                    "--commit" => {
                        commit = Some(value(rest, index, "--commit")?);
                        index += 2;
                    }
                    other => return Err(format!("unknown review option {other}")),
                }
            }
            Ok(Request::Review { id, pr, commit })
        }
        "close" => {
            let id = positional(rest, 0, "close ID")?;
            let mut reason = None;
            let mut evidence = Vec::new();
            let mut index = 1;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--reason" => {
                        reason = Some(value(rest, index, "--reason")?);
                        index += 2;
                    }
                    "--pr" => {
                        evidence.push(Evidence::pr(value(rest, index, "--pr")?));
                        index += 2;
                    }
                    "--commit" => {
                        evidence.push(Evidence::commit(value(rest, index, "--commit")?));
                        index += 2;
                    }
                    "--ack" => {
                        evidence.push(Evidence::ack(value(rest, index, "--ack")?));
                        index += 2;
                    }
                    other => return Err(format!("unknown close option {other}")),
                }
            }
            Ok(Request::Close {
                id,
                reason,
                evidence,
            })
        }
        "supersede" => {
            let id = positional(rest, 0, "supersede ID --with ID")?;
            let mut with = None;
            let mut index = 1;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--with" => {
                        with = Some(value(rest, index, "--with")?);
                        index += 2;
                    }
                    other => return Err(format!("unknown supersede option {other}")),
                }
            }
            Ok(Request::Supersede {
                id,
                with: with.ok_or("supersede needs --with ID")?,
            })
        }
        "dep" => {
            let add = match rest.first().map(String::as_str) {
                Some("add") => true,
                Some("rm") => false,
                Some(other) => return Err(format!("unknown dep action {other}; use add or rm")),
                None => return Err("usage: mb dep add|rm CHILD PARENT".into()),
            };
            Ok(Request::Dep {
                add,
                child: positional(&rest[1..], 0, "dep CHILD PARENT")?,
                parent: positional(&rest[1..], 1, "dep CHILD PARENT")?,
            })
        }
        "label" => {
            let add = match rest.first().map(String::as_str) {
                Some("add") => true,
                Some("rm") => false,
                Some(other) => return Err(format!("unknown label action {other}; use add or rm")),
                None => return Err("usage: mb label add|rm ID LABEL".into()),
            };
            Ok(Request::Label {
                add,
                id: positional(&rest[1..], 0, "label ID LABEL")?,
                label: positional(&rest[1..], 1, "label ID LABEL")?,
            })
        }
        "sweep" => no_flags(rest, Request::Sweep),
        "stats" => no_flags(rest, Request::Stats),
        "projects" => no_flags(rest, Request::Projects),
        "doctor" => no_flags(rest, Request::Doctor),
        "project" => parse_project(rest),
        "folder" => parse_folder(rest),
        "goal" => parse_goal(rest),
        "epic" => parse_epic(rest),
        "task" => parse_task(rest),
        "burndown" => parse_burndown(rest),
        "portfolio" => parse_portfolio(rest),
        "plan" => parse_plan(rest),
        other => {
            if let Some((_, why)) = NATIVE_ONLY.iter().find(|(command, _)| *command == other) {
                return Err(format!(
                    "{other} needs the native `marbles` binary: {why}. This build is the \
                     `soil:records` Spore; commands here: {}",
                    COMMANDS.join(", ")
                ));
            }
            Err(format!(
                "unknown command {other}\ncommands here: {}",
                COMMANDS.join(", ")
            ))
        }
    }
}

fn parse_create(rest: &[String]) -> Result<CreateArgs, String> {
    let mut args = CreateArgs {
        title: positional(rest, 0, "create TITLE")?,
        description: String::new(),
        priority: 2,
        issue_type: "task".into(),
        parent: None,
        labels: Vec::new(),
        id: None,
        available_at: None,
        external_ref: None,
        metadata: None,
        plan: None,
        done_means: None,
        repo: None,
        conflict_group: None,
        risk: None,
        acceptance: None,
        estimate: None,
    };
    let mut index = 1;
    while let Some(arg) = rest.get(index) {
        match arg.as_str() {
            "-d" | "--description" => {
                args.description = value(rest, index, "--description")?;
                index += 2;
            }
            "-p" | "--priority" => {
                args.priority = value(rest, index, "--priority")?
                    .parse()
                    .map_err(|_| "invalid priority".to_string())?;
                index += 2;
            }
            "-t" | "--type" | "--issue-type" => {
                args.issue_type = value(rest, index, "--type")?;
                index += 2;
            }
            "-l" | "--labels" => {
                args.labels = split_csv(&value(rest, index, "--labels a,b")?);
                index += 2;
            }
            "--parent" => {
                args.parent = Some(value(rest, index, "--parent")?);
                index += 2;
            }
            "--id" => {
                args.id = Some(value(rest, index, "--id")?);
                index += 2;
            }
            "--available-at" => {
                args.available_at = Some(
                    value(rest, index, "--available-at")?
                        .parse()
                        .map_err(|_| "--available-at needs epoch seconds".to_string())?,
                );
                index += 2;
            }
            "--external-ref" => {
                args.external_ref = Some(value(rest, index, "--external-ref")?);
                index += 2;
            }
            "--metadata" => {
                args.metadata = Some(value(rest, index, "--metadata")?);
                index += 2;
            }
            "--plan" => {
                args.plan = Some(value(rest, index, "--plan")?);
                index += 2;
            }
            "--done-means" => {
                args.done_means = Some(value(rest, index, "--done-means")?);
                index += 2;
            }
            "--repo" => {
                args.repo = Some(value(rest, index, "--repo")?);
                index += 2;
            }
            "--conflict-group" => {
                args.conflict_group = Some(value(rest, index, "--conflict-group")?);
                index += 2;
            }
            "--risk" => {
                args.risk = Some(value(rest, index, "--risk")?);
                index += 2;
            }
            "--acceptance" => {
                args.acceptance = Some(value(rest, index, "--acceptance")?);
                index += 2;
            }
            "--estimate" => {
                args.estimate = Some(
                    value(rest, index, "--estimate")?
                        .parse()
                        .map_err(|_| "--estimate needs minutes".to_string())?,
                );
                index += 2;
            }
            other => return Err(format!("unknown create option {other}")),
        }
    }
    Ok(args)
}

fn parse_update(rest: &[String]) -> Result<UpdateArgs, String> {
    let mut args = UpdateArgs {
        id: positional(rest, 0, "update ID")?,
        title: None,
        description: None,
        status: None,
        priority: None,
        append_notes: None,
        assignee: None,
        metadata: None,
        set_metadata: None,
    };
    let mut index = 1;
    while let Some(arg) = rest.get(index) {
        match arg.as_str() {
            "--title" => {
                args.title = Some(value(rest, index, "--title")?);
                index += 2;
            }
            "--description" => {
                args.description = Some(value(rest, index, "--description")?);
                index += 2;
            }
            "--status" => {
                args.status = Some(value(rest, index, "--status")?);
                index += 2;
            }
            "--priority" => {
                args.priority = Some(
                    value(rest, index, "--priority")?
                        .parse()
                        .map_err(|_| "invalid priority".to_string())?,
                );
                index += 2;
            }
            "--append-notes" => {
                args.append_notes = Some(value(rest, index, "--append-notes")?);
                index += 2;
            }
            "--assignee" => {
                args.assignee = Some(value(rest, index, "--assignee")?);
                index += 2;
            }
            "--metadata" => {
                args.metadata = Some(value(rest, index, "--metadata")?);
                index += 2;
            }
            "--set-metadata" => {
                let pair = value(rest, index, "--set-metadata")?;
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("--set-metadata wants key=value, got {pair}"))?;
                args.set_metadata = Some((key.to_string(), value.to_string()));
                index += 2;
            }
            other => return Err(format!("unknown update option {other}")),
        }
    }
    Ok(args)
}

fn parse_project(rest: &[String]) -> Result<Request, String> {
    match rest.first().map(String::as_str) {
        Some("rm") => Ok(Request::ProjectRemove {
            slug: positional(&rest[1..], 0, "project rm SLUG")?,
        }),
        Some(other) => Err(format!("unknown project action {other}; use rm")),
        None => Err("usage: mb project rm SLUG".into()),
    }
}

fn parse_folder(rest: &[String]) -> Result<Request, String> {
    match rest.first().map(String::as_str) {
        Some("create") => {
            let name = positional(&rest[1..], 0, "folder create NAME")?;
            let mut slug = None;
            let mut parent = None;
            let mut index = 2;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--slug" => {
                        slug = Some(value(rest, index, "--slug")?);
                        index += 2;
                    }
                    "--parent" => {
                        parent = Some(value(rest, index, "--parent")?);
                        index += 2;
                    }
                    other => return Err(format!("unknown folder create option {other}")),
                }
            }
            Ok(Request::Folder(FolderAction::Create { name, slug, parent }))
        }
        Some("list") => no_flags(&rest[1..], Request::Folder(FolderAction::List)),
        Some("tree") => no_flags(&rest[1..], Request::Folder(FolderAction::Tree)),
        Some("move") => {
            let project = positional(&rest[1..], 0, "folder move PROJECT FOLDER")?;
            let folder = positional(&rest[1..], 1, "folder move PROJECT FOLDER")?;
            Ok(Request::Folder(FolderAction::Move {
                project,
                folder: unfile_target(&folder),
            }))
        }
        Some(other) => Err(format!(
            "unknown folder action {other}; use create/list/tree/move"
        )),
        None => Err("usage: mb folder create|list|tree|move".into()),
    }
}

/// `none`, `-`, or an empty string unfiles a project; anything else is a folder.
fn unfile_target(folder: &str) -> Option<String> {
    if folder == "none" || folder == "-" || folder.is_empty() {
        None
    } else {
        Some(folder.to_string())
    }
}

fn parse_goal(rest: &[String]) -> Result<Request, String> {
    match rest.first().map(String::as_str) {
        Some("create") => Ok(Request::Goal(GoalAction::Create(Box::new(
            parse_work_create(&rest[1..], "goal create TITLE")?,
        )))),
        Some("list") => {
            let mut all = false;
            let mut index = 1;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--all" => {
                        all = true;
                        index += 1;
                    }
                    other => return Err(format!("unknown goal list option {other}")),
                }
            }
            Ok(Request::Goal(GoalAction::List { all }))
        }
        Some("show") => Ok(Request::Goal(GoalAction::Show {
            id: positional(&rest[1..], 0, "goal show ID")?,
        })),
        Some(other) => Err(format!("unknown goal action {other}; use create/list/show")),
        None => Err("usage: mb goal create|list|show".into()),
    }
}

fn parse_epic(rest: &[String]) -> Result<Request, String> {
    match rest.first().map(String::as_str) {
        Some("create") => {
            let mut goal = None;
            let mut title = None;
            let mut description = String::new();
            let mut priority = 2;
            let mut labels = Vec::new();
            let mut available_at = None;
            let mut index = 1;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--goal" => {
                        goal = Some(value(rest, index, "--goal")?);
                        index += 2;
                    }
                    "-d" | "--description" => {
                        description = value(rest, index, "--description")?;
                        index += 2;
                    }
                    "-p" | "--priority" => {
                        priority = value(rest, index, "--priority")?
                            .parse()
                            .map_err(|_| "invalid priority".to_string())?;
                        index += 2;
                    }
                    "-l" | "--labels" => {
                        labels = split_csv(&value(rest, index, "--labels a,b")?);
                        index += 2;
                    }
                    "--available-at" => {
                        available_at = Some(
                            value(rest, index, "--available-at")?
                                .parse()
                                .map_err(|_| "--available-at needs epoch seconds".to_string())?,
                        );
                        index += 2;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unknown epic create option {other}"));
                    }
                    other => {
                        if title.replace(other.to_string()).is_some() {
                            return Err("epic create takes one title".into());
                        }
                        index += 1;
                    }
                }
            }
            let goal = goal.ok_or("epic create needs --goal ID")?;
            let title = title.ok_or("usage: mb epic create --goal ID TITLE")?;
            Ok(Request::Epic(EpicAction::Create {
                goal,
                args: WorkCreateArgs {
                    title,
                    description,
                    priority,
                    labels,
                    available_at,
                },
            }))
        }
        Some(other) => Err(format!("unknown epic action {other}; use create")),
        None => Err("usage: mb epic create --goal ID TITLE".into()),
    }
}

fn parse_task(rest: &[String]) -> Result<Request, String> {
    match rest.first().map(String::as_str) {
        Some("create") => {
            let mut epic = None;
            let mut title = None;
            let mut description = String::new();
            let mut priority = 2;
            let mut labels = Vec::new();
            let mut available_at = None;
            let mut index = 1;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--epic" => {
                        epic = Some(value(rest, index, "--epic")?);
                        index += 2;
                    }
                    "-d" | "--description" => {
                        description = value(rest, index, "--description")?;
                        index += 2;
                    }
                    "-p" | "--priority" => {
                        priority = value(rest, index, "--priority")?
                            .parse()
                            .map_err(|_| "invalid priority".to_string())?;
                        index += 2;
                    }
                    "-l" | "--labels" => {
                        labels = split_csv(&value(rest, index, "--labels a,b")?);
                        index += 2;
                    }
                    "--available-at" => {
                        available_at = Some(
                            value(rest, index, "--available-at")?
                                .parse()
                                .map_err(|_| "--available-at needs epoch seconds".to_string())?,
                        );
                        index += 2;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unknown task create option {other}"));
                    }
                    other => {
                        if title.replace(other.to_string()).is_some() {
                            return Err("task create takes one title".into());
                        }
                        index += 1;
                    }
                }
            }
            let epic = epic.ok_or("task create needs --epic ID")?;
            let title = title.ok_or("usage: mb task create --epic ID TITLE")?;
            Ok(Request::Task(TaskAction::Create {
                epic,
                args: WorkCreateArgs {
                    title,
                    description,
                    priority,
                    labels,
                    available_at,
                },
            }))
        }
        Some(other) => Err(format!("unknown task action {other}; use create")),
        None => Err("usage: mb task create --epic ID TITLE".into()),
    }
}

fn parse_work_create(rest: &[String], usage: &str) -> Result<WorkCreateArgs, String> {
    let mut args = WorkCreateArgs {
        title: positional(rest, 0, usage)?,
        description: String::new(),
        priority: 2,
        labels: Vec::new(),
        available_at: None,
    };
    let mut index = 1;
    while let Some(arg) = rest.get(index) {
        match arg.as_str() {
            "-d" | "--description" => {
                args.description = value(rest, index, "--description")?;
                index += 2;
            }
            "-p" | "--priority" => {
                args.priority = value(rest, index, "--priority")?
                    .parse()
                    .map_err(|_| "invalid priority".to_string())?;
                index += 2;
            }
            "-l" | "--labels" => {
                args.labels = split_csv(&value(rest, index, "--labels a,b")?);
                index += 2;
            }
            "--available-at" => {
                args.available_at = Some(
                    value(rest, index, "--available-at")?
                        .parse()
                        .map_err(|_| "--available-at needs epoch seconds".to_string())?,
                );
                index += 2;
            }
            other => return Err(format!("unknown create option {other}")),
        }
    }
    Ok(args)
}

fn parse_burndown(rest: &[String]) -> Result<Request, String> {
    let mut goal = None;
    let mut epic = None;
    let mut days = 30;
    let mut index = 0;
    while let Some(arg) = rest.get(index) {
        match arg.as_str() {
            "--goal" => {
                goal = Some(value(rest, index, "--goal")?);
                index += 2;
            }
            "--epic" => {
                epic = Some(value(rest, index, "--epic")?);
                index += 2;
            }
            "--days" => {
                days = value(rest, index, "--days")?
                    .parse()
                    .map_err(|_| "--days needs a number".to_string())?;
                index += 2;
            }
            other => return Err(format!("unknown burndown option {other}")),
        }
    }
    Ok(Request::Burndown { goal, epic, days })
}

fn parse_portfolio(rest: &[String]) -> Result<Request, String> {
    let mut folder = None;
    let mut days = 14;
    let mut stale_days = 7;
    let mut index = 0;
    while let Some(arg) = rest.get(index) {
        match arg.as_str() {
            "--folder" => {
                folder = Some(value(rest, index, "--folder")?);
                index += 2;
            }
            "--days" => {
                days = value(rest, index, "--days")?
                    .parse()
                    .map_err(|_| "--days needs a number".to_string())?;
                index += 2;
            }
            "--stale-days" => {
                stale_days = value(rest, index, "--stale-days")?
                    .parse()
                    .map_err(|_| "--stale-days needs a number".to_string())?;
                index += 2;
            }
            other => return Err(format!("unknown portfolio option {other}")),
        }
    }
    Ok(Request::Portfolio {
        folder,
        days,
        stale_days,
    })
}

fn parse_plan(rest: &[String]) -> Result<Request, String> {
    match rest.first().map(String::as_str) {
        Some("show") => Ok(Request::Plan(PlanAction::Show {
            plan: positional(&rest[1..], 0, "plan show PLAN")?,
        })),
        Some("gantt") => Ok(Request::Plan(PlanAction::Gantt {
            plan: positional(&rest[1..], 0, "plan gantt PLAN")?,
        })),
        Some("ready") => Ok(Request::Plan(PlanAction::Ready {
            plan: positional(&rest[1..], 0, "plan ready PLAN")?,
        })),
        Some("create") => {
            let spec = positional(&rest[1..], 0, "plan create SPEC")?;
            let mut plan = None;
            let mut project = None;
            let mut dry_run = false;
            let mut index = 2;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--plan" => {
                        plan = Some(value(rest, index, "--plan")?);
                        index += 2;
                    }
                    "--project" => {
                        project = Some(value(rest, index, "--project")?);
                        index += 2;
                    }
                    "--dry-run" => {
                        dry_run = true;
                        index += 1;
                    }
                    other => return Err(format!("unknown plan create option {other}")),
                }
            }
            Ok(Request::Plan(PlanAction::Create {
                spec,
                plan,
                project,
                dry_run,
            }))
        }
        Some(other) => Err(format!(
            "unknown plan action {other}; use show/gantt/ready/create"
        )),
        None => Err("usage: mb plan show|gantt|ready|create".into()),
    }
}

fn no_flags(rest: &[String], request: Request) -> Result<Request, String> {
    match rest.first() {
        None => Ok(request),
        Some(other) => Err(format!("unexpected argument {other}")),
    }
}

/// `--as NAME` for the holder of a claim.
fn holder_flag(rest: &[String], command: &str) -> Result<Option<String>, String> {
    let mut holder = None;
    let mut index = 1;
    while let Some(arg) = rest.get(index) {
        match arg.as_str() {
            "--as" => {
                holder = Some(value(rest, index, "--as")?);
                index += 2;
            }
            other => return Err(format!("unknown {command} option {other}")),
        }
    }
    Ok(holder)
}

fn split_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .collect()
}

fn value(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// An empty variable is not a value.
///
/// A host resolves what it knows and passes it on; a shell shim that found no
/// project passes `MARBLES_PROJECT=` rather than building two argument lists.
/// Reading that as "scope to the project whose slug is the empty string" would
/// answer an empty list and look like an empty store, so it reads as unset.
fn lookup(env: &BTreeMap<String, String>, key: &str) -> Option<String> {
    env.get(key).filter(|value| !value.is_empty()).cloned()
}

fn positional(args: &[String], index: usize, what: &str) -> Result<String, String> {
    args.get(index)
        .cloned()
        .ok_or_else(|| format!("usage: mb {what}"))
}

fn usage() -> String {
    format!(
        "usage: mb [--project SLUG] [--actor NAME] [--json] <command>\ncommands here: {}",
        COMMANDS.join(", ")
    )
}
