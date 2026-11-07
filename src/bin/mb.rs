//! Minimal wasm `mb` CLI over `soil:records`.

use std::process::ExitCode;

use marbles::adapter::Adapter;
use marbles::store::{self, Store};
use marbles::types::{ActorKind, ClaimRequest, Evidence, NewIssue};

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut project = std::env::var("MARBLES_PROJECT").ok();
    let mut prefix = std::env::var("MARBLES_PREFIX").ok();
    let mut actor = std::env::var("MARBLES_ACTOR")
        .ok()
        .unwrap_or_else(|| "agent".into());
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--project" => {
                project = args.get(index + 1).cloned();
                index += 2;
            }
            "--prefix" => {
                prefix = args.get(index + 1).cloned();
                index += 2;
            }
            "--actor" => {
                actor = args
                    .get(index + 1)
                    .cloned()
                    .ok_or("--actor needs a value")?;
                index += 2;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option {other}"));
            }
            _ => {
                rest.extend_from_slice(&args[index..]);
                break;
            }
        }
    }
    if rest.is_empty() {
        return Err("usage: mb --project SLUG [command]".into());
    }
    let project = project.unwrap_or_else(|| "default".into());
    let prefix = prefix.unwrap_or_else(|| project.clone());
    let store = Adapter::open("wasm".into()).map_err(|error| error.to_string())?;
    if rest[0] == "reindex" {
        let (projects, dependencies) = store.reindex().map_err(err)?;
        print_json(&serde_json::json!({
            "projects": projects,
            "dependencies": dependencies
        }))?;
        return Ok(());
    }
    let now = store::now();
    let known = store
        .projects()
        .map_err(err)?
        .iter()
        .any(|(slug, _, _)| slug == &project);
    if !known {
        store
            .ensure_project(&project, "", &prefix)
            .map_err(|error| error.to_string())?;
    }
    match rest[0].as_str() {
        "list" => print_json(&store.list(Some(&project), false, None).map_err(err)?)?,
        "ready" => print_json(&store.ready(Some(&project), now).map_err(err)?)?,
        "show" => {
            let id = rest.get(1).ok_or("show ID")?;
            print_json(&store.get(id).map_err(err)?)?
        }
        "create" => {
            let title = rest.get(1).ok_or("create TITLE")?;
            let mut issue = NewIssue {
                title: title.clone(),
                project: Some(project.clone()),
                created_by: Some(actor.clone()),
                ..Default::default()
            };
            let mut i = 2;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--description" => {
                        issue.description = rest.get(i + 1).cloned().ok_or("--description")?;
                        i += 2;
                    }
                    "--priority" => {
                        issue.priority = rest
                            .get(i + 1)
                            .ok_or("--priority")?
                            .parse()
                            .map_err(|_| "invalid priority")?;
                        i += 2;
                    }
                    "--type" => {
                        issue.issue_type = rest.get(i + 1).cloned().ok_or("--type")?;
                        i += 2;
                    }
                    "--parent" => {
                        issue.parent = Some(rest.get(i + 1).cloned().ok_or("--parent")?);
                        i += 2;
                    }
                    other => return Err(format!("unknown create option {other}")),
                }
            }
            let id = store.create(&issue, now).map_err(err)?;
            print_json(&serde_json::json!({"id": id}))?
        }
        "claim" => {
            let id = rest.get(1).ok_or("claim ID")?;
            let kind = if rest.iter().any(|arg| arg == "--human") {
                ActorKind::Human
            } else {
                ActorKind::Agent
            };
            let receipt = store
                .claim(
                    &ClaimRequest {
                        id: id.clone(),
                        assignee: actor.clone(),
                        actor_kind: kind,
                        ttl_seconds: None,
                    },
                    now,
                )
                .map_err(err)?;
            print_json(&receipt)?
        }
        "release" => {
            let id = rest.get(1).ok_or("release ID")?;
            store.release(id, &actor, now).map_err(err)?;
            print_json(&serde_json::json!({"released": id}))?
        }
        "close" => {
            let id = rest.get(1).ok_or("close ID")?;
            let mut reason = None;
            let mut evidence = Vec::new();
            let mut i = 2;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--reason" => {
                        reason = Some(rest.get(i + 1).cloned().ok_or("--reason")?);
                        i += 2;
                    }
                    "--pr" => {
                        evidence.push(Evidence::pr(rest.get(i + 1).cloned().ok_or("--pr")?));
                        i += 2;
                    }
                    "--commit" => {
                        evidence.push(Evidence::commit(
                            rest.get(i + 1).cloned().ok_or("--commit")?,
                        ));
                        i += 2;
                    }
                    "--ack" => {
                        evidence.push(Evidence::ack(rest.get(i + 1).cloned().ok_or("--ack")?));
                        i += 2;
                    }
                    other => return Err(format!("unknown close option {other}")),
                }
            }
            let issue = store
                .close(id, reason.as_deref(), &evidence, &actor, now)
                .map_err(err)?;
            print_json(&issue)?
        }
        "history" => {
            let id = rest.get(1).ok_or("history ID")?;
            print_json(&store.history(id).map_err(err)?)?
        }
        other => return Err(format!("unknown command {other}")),
    }
    Ok(())
}

fn err(error: marbles::store::Error) -> String {
    error.to_string()
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(())
}
