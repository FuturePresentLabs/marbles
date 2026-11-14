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
//! to tell which backend answered except by which commands exist.

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
}

/// Commands this backend answers. Named so the failure for anything else can
/// say what does work instead of echoing an unknown-word error.
pub const COMMANDS: &[&str] = &[
    "claim",
    "close",
    "create",
    "dep",
    "history",
    "label",
    "list",
    "projects",
    "ready",
    "release",
    "review",
    "show",
    "stats",
    "supersede",
    "sweep",
    "touch",
];

/// Flags that belong to the server/HTTP half of Marbles and have no meaning
/// against a records store the host already selected. Rejecting them by name
/// beats accepting them and quietly ignoring where the caller pointed.
const TRANSPORT_FLAGS: &[&str] = &["--url", "--token", "--token-file", "--timeout-ms"];

pub fn parse(argv: &[String], env: &BTreeMap<String, String>) -> Result<Invocation, String> {
    // `--json` is `global = true` in the native CLI, so `mb ready --json` is a
    // documented spelling and cannot be a leading-only flag. Every answer here
    // is already JSON, so the flag needs no state beyond being accepted.
    let argv = argv
        .iter()
        .filter(|arg| arg.as_str() != "--json")
        .cloned()
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
        "create" => {
            let mut args = CreateArgs {
                title: positional(rest, 0, "create TITLE")?,
                description: String::new(),
                priority: 0,
                issue_type: String::new(),
                parent: None,
                labels: Vec::new(),
            };
            let mut index = 1;
            while let Some(arg) = rest.get(index) {
                match arg.as_str() {
                    "--description" => {
                        args.description = value(rest, index, "--description")?;
                        index += 2;
                    }
                    "--priority" => {
                        args.priority = value(rest, index, "--priority")?
                            .parse()
                            .map_err(|_| "invalid priority".to_string())?;
                        index += 2;
                    }
                    "--type" => {
                        args.issue_type = value(rest, index, "--type")?;
                        index += 2;
                    }
                    "--parent" => {
                        args.parent = Some(value(rest, index, "--parent")?);
                        index += 2;
                    }
                    "--labels" => {
                        args.labels = value(rest, index, "--labels a,b")?
                            .split(',')
                            .map(str::trim)
                            .filter(|label| !label.is_empty())
                            .map(str::to_owned)
                            .collect();
                        index += 2;
                    }
                    other => return Err(format!("unknown create option {other}")),
                }
            }
            Ok(Request::Create(Box::new(args)))
        }
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
        other => Err(format!(
            "unknown command {other}\ncommands here: {}",
            COMMANDS.join(", ")
        )),
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
