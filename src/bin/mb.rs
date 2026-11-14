//! `mb`: the Marbles CLI over `soil:records`.
//!
//! The host decides which records book and collection this runs against; this
//! binary names only the `marbles` collection and the record schemas, exactly as
//! `docs/peer-sync.md` requires of a Spore. Everything that selects an endpoint,
//! a token or a filesystem path belongs to the host that launched it.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::ExitCode;

use marbles::adapter::Adapter;
use marbles::records_cli::{self, CreateArgs, Invocation, Request};
use marbles::store::{self, Store};
use marbles::types::{ClaimRequest, Evidence, IssuePatch, NewIssue};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mb: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env = MARBLES_ENV
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (*key, value)))
        .map(|(key, value)| (key.to_string(), value))
        .collect::<BTreeMap<_, _>>();
    let invocation = records_cli::parse(&argv, &env)?;
    let store = Adapter::open("marbles".into()).map_err(|error| error.to_string())?;
    let now = store::now();
    execute(&store, &invocation, now)
}

/// The only environment a caller can use to scope an invocation. A Spore has no
/// ambient environment; these three are the documented `mb` contract, and the
/// host is what puts them there.
const MARBLES_ENV: &[&str] = &["MARBLES_PROJECT", "MARBLES_PREFIX", "MARBLES_ACTOR"];

fn execute(store: &Adapter, invocation: &Invocation, now: i64) -> Result<(), String> {
    let actor = &invocation.actor;
    let project = invocation.project.as_deref();
    match &invocation.request {
        Request::List { all, id, limit } => {
            let mut issues = store
                .list(project, *all, id.as_deref())
                .map_err(err)?;
            if *limit > 0 {
                issues.truncate(*limit as usize);
            }
            print_json(&issues)
        }
        Request::Ready => print_json(&store.ready(project, now).map_err(err)?),
        // A one-element array: the native CLI prints this shape because Beads
        // clients parse `show --json` as a list, and `soil-marbles` reads it that
        // way too. The records backend answers the same contract.
        Request::Show { id } => print_json(&[store.get(id).map_err(err)?]),
        Request::Create(args) => {
            let slug = project.ok_or_else(|| {
                "create needs a project: pass --project SLUG or set MARBLES_PROJECT".to_string()
            })?;
            ensure_project(store, slug, invocation.prefix.as_deref())?;
            let id = store
                .create(&new_issue(args, slug, actor), now)
                .map_err(err)?;
            print_json(&serde_json::json!({ "id": id }))
        }
        Request::Claim {
            id,
            kind,
            ttl_seconds,
            assignee,
        } => {
            let receipt = store
                .claim(
                    &ClaimRequest {
                        id: id.clone(),
                        assignee: assignee.clone().unwrap_or_else(|| actor.clone()),
                        actor_kind: *kind,
                        ttl_seconds: *ttl_seconds,
                    },
                    now,
                )
                .map_err(err)?;
            print_json(&receipt)
        }
        Request::Touch { id, holder } => {
            let expires_at = store
                .touch(id, holder.as_deref().unwrap_or(actor), now)
                .map_err(err)?;
            print_json(&serde_json::json!({ "expires_at": expires_at }))
        }
        Request::Release { id, holder } => {
            store
                .release(id, holder.as_deref().unwrap_or(actor), now)
                .map_err(err)?;
            print_json(&serde_json::json!({ "released": id }))
        }
        Request::Review { id, pr, commit } => {
            // Review is a state, not a note: keep the evidence already recorded
            // and replace only the same kind, so re-running `review --pr`
            // corrects the URL instead of stacking two.
            let existing = store.get(id).map_err(err)?;
            let mut evidence = existing.evidence.clone();
            if let Some(pr) = pr {
                evidence.retain(|entry| entry.kind != "pr");
                evidence.push(Evidence::pr(pr.clone()));
            }
            if let Some(commit) = commit {
                evidence.retain(|entry| entry.kind != "commit");
                evidence.push(Evidence::commit(commit.clone()));
            }
            let issue = store
                .update(
                    id,
                    &IssuePatch {
                        status: Some("review".into()),
                        evidence: Some(evidence),
                        ..Default::default()
                    },
                    actor,
                    now,
                )
                .map_err(err)?;
            print_json(&issue)
        }
        Request::Close {
            id,
            reason,
            evidence,
        } => {
            let issue = store
                .close(id, reason.as_deref(), evidence, actor, now)
                .map_err(err)?;
            print_json(&issue)
        }
        Request::Supersede { id, with } => {
            store.supersede(id, with, actor, now).map_err(err)?;
            print_json(&serde_json::json!({ "superseded": id, "by": with }))
        }
        Request::Dep { add, child, parent } => {
            if *add {
                store.add_dep(child, parent, actor, now).map_err(err)?;
            } else {
                store.remove_dep(child, parent, actor, now).map_err(err)?;
            }
            print_json(&serde_json::json!({ "child": child, "parent": parent, "present": add }))
        }
        Request::Label { add, id, label } => {
            if *add {
                store.add_label(id, label, actor, now).map_err(err)?;
            } else {
                store.remove_label(id, label, actor, now).map_err(err)?;
            }
            print_json(&serde_json::json!({ "id": id, "label": label, "present": add }))
        }
        Request::History { id } => print_json(&store.history(id).map_err(err)?),
        Request::Sweep => print_json(&store.sweep(now).map_err(err)?),
        Request::Stats => print_json(&store.stats().map_err(err)?),
        Request::Projects => print_json(
            &store
                .projects()
                .map_err(err)?
                .into_iter()
                .map(|(slug, root, prefix)| {
                    serde_json::json!({ "slug": slug, "root": root, "prefix": prefix })
                })
                .collect::<Vec<_>>(),
        ),
    }
}

/// Register the project on first write.
///
/// A records store is shared by every project a machine has ever tracked, so the
/// project row cannot be created by `mb init` alone: an import or a first
/// `create` from a fresh checkout has to be able to establish it. The prefix
/// defaults to the slug, which is the same rule the native CLI applies.
fn ensure_project(store: &Adapter, slug: &str, prefix: Option<&str>) -> Result<(), String> {
    let known = store
        .projects()
        .map_err(err)?
        .iter()
        .any(|(existing, _, _)| existing == slug);
    if !known {
        store
            .ensure_project(slug, "", prefix.unwrap_or(slug))
            .map_err(err)?;
    }
    Ok(())
}

fn new_issue(args: &CreateArgs, project: &str, actor: &str) -> NewIssue {
    NewIssue {
        title: args.title.clone(),
        description: args.description.clone(),
        issue_type: args.issue_type.clone(),
        priority: args.priority,
        labels: args.labels.clone(),
        parent: args.parent.clone(),
        project: Some(project.to_string()),
        created_by: Some(actor.to_string()),
        ..Default::default()
    }
}

fn err(error: marbles::store::Error) -> String {
    error.to_string()
}

fn print_json<T: serde::Serialize + ?Sized>(value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    // `println!` panics when the write fails, and a host caps a command's stdout
    // with a pipe: `mb list --all` over a fleet-sized store hits that cap, and a
    // panic turns a size limit into a wasm trap with a backtrace for an error
    // message. Report the cap and what narrows the answer instead.
    let mut stdout = std::io::stdout();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|error| {
            format!(
                "could not write the answer ({error}); the host caps a command's stdout, so \
                 narrow the query with --project or --limit"
            )
        })
}
