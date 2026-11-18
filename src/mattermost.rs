//! Mattermost slash commands: one HTTP door, the same store.
//!
//! Mattermost posts a form-encoded slash command to [`SLASH_PATH`]. The command
//! token in that form is a *deployment* credential, held by the Mattermost
//! server; once it verifies, the form's `user_name` is the Marbles principal.
//! That is the honest boundary: Mattermost authenticated the person, Marbles
//! records what that person asked for. A slash command is not a second store,
//! a second writer, or a bot that can claim on anybody's behalf.
//!
//! This module is an adapter. It parses a small command grammar, calls the same
//! `Store` operations every other client uses, and renders Mattermost's JSON
//! response envelope. There is deliberately no `/marbles/issues` REST surface
//! here: the canonical API remains `/v1/issues.*`, and an external client
//! should use it rather than imitating a chat provider.

use std::sync::Arc;

use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::api::Api;
use crate::auth::Principal;
use crate::config::MattermostConfig;
use crate::db::now;
use crate::types::*;

/// The one route Mattermost needs. Configure the slash command's request URL as
/// `<marbles-base>/v1/mattermost/slash`.
pub const SLASH_PATH: &str = "/v1/mattermost/slash";

struct MattermostState {
    api: Arc<Api>,
    config: MattermostConfig,
    token: String,
}

/// Build the slash-command router. The command token is read once at startup;
/// an empty or unreadable file fails startup rather than accepting every form.
pub fn router(api: Arc<Api>, config: MattermostConfig) -> Result<Router, String> {
    let token = std::fs::read_to_string(&config.token_file).map_err(|e| {
        format!(
            "read Mattermost slash-command token {}: {e}",
            config.token_file.display()
        )
    })?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(format!(
            "Mattermost slash-command token {} is empty",
            config.token_file.display()
        ));
    }
    let state = Arc::new(MattermostState { api, config, token });
    Ok(Router::new()
        .route(SLASH_PATH, post(slash))
        .with_state(state))
}

#[derive(Debug, Deserialize)]
struct SlashPayload {
    #[serde(default)]
    token: String,
    #[serde(default)]
    user_name: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Serialize)]
struct SlashResponse {
    response_type: String,
    text: String,
}

#[derive(Debug)]
struct Reply {
    response_type: &'static str,
    text: String,
}

impl Reply {
    fn ephemeral(text: impl Into<String>) -> Self {
        Self {
            response_type: "ephemeral",
            text: text.into(),
        }
    }

    fn in_channel(text: impl Into<String>) -> Self {
        Self {
            response_type: "in_channel",
            text: text.into(),
        }
    }
}

async fn slash(
    State(state): State<Arc<MattermostState>>,
    Form(payload): Form<SlashPayload>,
) -> Response {
    // Mattermost's token is a shared deployment secret, not a person. Compare it
    // without an early return so a wrong token does not leak its prefix.
    if !constant_time_eq(payload.token.as_bytes(), state.token.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            "invalid Mattermost slash-command token",
        )
            .into_response();
    }

    let user = payload.user_name.trim();
    if user.is_empty() {
        return json_reply(Reply::ephemeral(
            "Marbles: Mattermost did not send a user name.",
        ));
    }
    if !state.config.allowed_users.is_empty()
        && !state
            .config
            .allowed_users
            .iter()
            .any(|allowed| allowed == user)
    {
        return json_reply(Reply::ephemeral(format!(
            "Marbles: {user} is not allowed to use this slash command."
        )));
    }

    let principal = Principal {
        kind: ActorKind::Human,
        name: user.to_string(),
        company_id: state.config.company.clone(),
    };
    let command = match parse_command(&payload.text) {
        Ok(command) => command,
        Err(message) => return json_reply(Reply::ephemeral(format!("Marbles: {message}"))),
    };

    let state = Arc::clone(&state);
    match tokio::task::spawn_blocking(move || execute(&state, &principal, command)).await {
        Ok(Ok(reply)) => json_reply(reply),
        Ok(Err(message)) => json_reply(Reply::ephemeral(format!("Marbles: {message}"))),
        Err(_) => json_reply(Reply::ephemeral(
            "Marbles: the storage worker stopped before answering.",
        )),
    }
}

fn json_reply(reply: Reply) -> Response {
    Json(SlashResponse {
        response_type: reply.response_type.to_string(),
        text: reply.text,
    })
    .into_response()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

fn execute(
    state: &MattermostState,
    principal: &Principal,
    command: Command,
) -> Result<Reply, String> {
    let store = state.api.db_for(principal).map_err(|e| e.to_string())?;
    let at = now();

    match command {
        Command::Help => Ok(Reply::ephemeral(HELP)),
        Command::Create {
            title,
            description,
            project,
            priority,
            issue_type,
            labels,
            parent,
        } => {
            let project = project
                .or_else(|| state.config.default_project.clone())
                .ok_or_else(|| {
                    "create needs --project (or [mattermost] default_project)".to_string()
                })?;
            let spec = NewIssue {
                title,
                description,
                issue_type: if issue_type.is_empty() {
                    "task".to_string()
                } else {
                    issue_type
                },
                priority,
                labels,
                parent,
                project: Some(project),
                created_by: Some(principal.name.clone()),
                ..NewIssue::default()
            };
            let id = store.create(&spec, at).map_err(|e| e.to_string())?;
            Ok(Reply::in_channel(format!(
                "Created **{id}** — {}\nproject {} · {}",
                spec.title,
                spec.project.as_deref().unwrap_or(""),
                spec.issue_type,
            )))
        }
        Command::List {
            project,
            status,
            all,
            limit,
            query,
        } => {
            if let Some(status) = &status
                && !STATUSES.contains(&status.as_str())
            {
                return Err(format!("--status must be one of {}", STATUSES.join(", ")));
            }
            // `list` already means "not closed". Asking for `closed` is the one
            // status filter that needs the all-inclusive scan.
            let include_closed = all || status.as_deref() == Some("closed");
            let issues = store
                .list(project.as_deref(), include_closed, None)
                .map_err(|e| e.to_string())?;
            let needle = query.as_deref().map(str::to_lowercase);
            let filtered: Vec<Issue> = issues
                .into_iter()
                .filter(|issue| status.as_deref().is_none_or(|s| issue.status == s))
                .filter(|issue| {
                    needle.as_ref().is_none_or(|needle| {
                        issue.id.to_lowercase().contains(needle)
                            || issue.title.to_lowercase().contains(needle)
                            || issue.description.to_lowercase().contains(needle)
                    })
                })
                .collect();
            let total = filtered.len();
            let limit = limit.clamp(1, 50);
            let mut text = String::new();
            if let Some(project) = &project {
                text.push_str(&format!("**{project}**"));
                if let Some(status) = &status {
                    text.push_str(&format!(" · {status}"));
                }
                text.push('\n');
            }
            for issue in filtered.iter().take(limit) {
                text.push_str(&format!(
                    "- `{}` {} · P{} — {}\n",
                    issue.id, issue.status, issue.priority, issue.title
                ));
            }
            if total == 0 {
                text.push_str("No matching issues.");
            } else if total > limit {
                text.push_str(&format!("…and {} more.\n", total - limit));
            }
            Ok(Reply::ephemeral(text.trim_end().to_string()))
        }
        Command::Ready { project, limit } => {
            let issues = store
                .ready(project.as_deref(), at)
                .map_err(|e| e.to_string())?;
            let total = issues.len();
            let limit = limit.clamp(1, 50);
            let mut text = String::new();
            if let Some(project) = &project {
                text.push_str(&format!("**{project}** ready\n"));
            }
            for issue in issues.iter().take(limit) {
                text.push_str(&format!(
                    "- `{}` P{} — {}\n",
                    issue.id, issue.priority, issue.title
                ));
            }
            if total == 0 {
                text.push_str("Nothing is ready.");
            } else if total > limit {
                text.push_str(&format!("…and {} more.\n", total - limit));
            }
            Ok(Reply::ephemeral(text.trim_end().to_string()))
        }
        Command::Show { id } => {
            let issue = store.get(&id).map_err(|e| e.to_string())?;
            Ok(Reply::ephemeral(format_issue(&issue)))
        }
        Command::Update {
            id,
            title,
            description,
            status,
            priority,
            note,
            labels_add,
            labels_remove,
        } => {
            if title.is_none()
                && description.is_none()
                && status.is_none()
                && priority.is_none()
                && note.is_none()
                && labels_add.is_empty()
                && labels_remove.is_empty()
            {
                return Err(
                    "update needs at least one of --title, --description, --status, --priority, \
                     --note, --label-add, --label-remove"
                        .to_string(),
                );
            }
            let patch = IssuePatch {
                title,
                description,
                status,
                priority,
                append_notes: note,
                ..IssuePatch::default()
            };
            store
                .update(&id, &patch, &principal.name, at)
                .map_err(|e| e.to_string())?;
            for label in labels_add {
                store
                    .add_label(&id, &label, &principal.name, at)
                    .map_err(|e| e.to_string())?;
            }
            for label in labels_remove {
                store
                    .remove_label(&id, &label, &principal.name, at)
                    .map_err(|e| e.to_string())?;
            }
            let issue = store.get(&id).map_err(|e| e.to_string())?;
            Ok(Reply::in_channel(format!(
                "Updated **{}** — {}\n`{}` · P{}",
                issue.id, issue.title, issue.status, issue.priority
            )))
        }
        Command::Claim { id } => {
            let receipt = store
                .claim(
                    &ClaimRequest {
                        id: id.clone(),
                        assignee: principal.name.clone(),
                        actor_kind: ActorKind::Human,
                        ttl_seconds: None,
                    },
                    at,
                )
                .map_err(|e| e.to_string())?;
            Ok(Reply::in_channel(format!(
                "Claimed **{}** as **{}** until {}",
                receipt.id,
                receipt.assignee,
                format_time(receipt.expires_at)
            )))
        }
        Command::Release { id } => {
            let issue = store.get(&id).map_err(|e| e.to_string())?;
            if let Some(holder) = &issue.assignee
                && holder != &principal.name
            {
                return Err(format!("{id} is held by {holder}"));
            }
            store
                .release(&id, &principal.name, at)
                .map_err(|e| e.to_string())?;
            Ok(Reply::in_channel(format!("Released **{id}**.")))
        }
        Command::Close {
            id,
            reason,
            pr,
            commit,
            ack,
        } => {
            let mut evidence = Vec::new();
            if let Some(pr) = pr {
                evidence.push(Evidence::pr(pr));
            }
            if let Some(commit) = commit {
                evidence.push(Evidence::commit(commit));
            }
            if let Some(ack) = ack {
                evidence.push(Evidence::ack(ack));
            }
            if evidence.is_empty() {
                return Err(
                    "close needs evidence: --pr URL, --commit SHA, or --ack \"why nothing shipped\""
                        .to_string(),
                );
            }
            let issue = store
                .close(&id, reason.as_deref(), &evidence, &principal.name, at)
                .map_err(|e| e.to_string())?;
            Ok(Reply::in_channel(format!(
                "Closed **{}** — {}\nEvidence: {}",
                issue.id,
                issue.title,
                issue
                    .evidence
                    .iter()
                    .map(|e| format!("{}:{}", e.kind, e.value))
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    }
}

fn format_issue(issue: &Issue) -> String {
    let mut text = format!(
        "**{}** — {}\n`{}` · P{}",
        issue.id, issue.title, issue.status, issue.priority
    );
    if !issue.issue_type.is_empty() {
        text.push_str(&format!(" · {}", issue.issue_type));
    }
    if !issue.project.is_empty() {
        text.push_str(&format!(" · project {}", issue.project));
    }
    if let Some(assignee) = &issue.assignee {
        text.push_str(&format!(" · claimed by {assignee}"));
    }
    if !issue.labels.is_empty() {
        text.push_str(&format!("\nlabels: {}", issue.labels.join(", ")));
    }
    if !issue.description.is_empty() {
        text.push_str(&format!("\n{}", truncate(&issue.description, 500)));
    }
    if issue.dependency_count > 0 {
        text.push_str(&format!("\nwaits on {} issue(s)", issue.dependency_count));
    }
    if !issue.evidence.is_empty() {
        text.push_str(&format!(
            "\nevidence: {}",
            issue
                .evidence
                .iter()
                .map(|e| format!("{}:{}", e.kind, e.value))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    text
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut out: String = value.chars().take(limit).collect();
    out.push('…');
    out
}

fn format_time(epoch: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0)
        .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| epoch.to_string())
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Create {
        title: String,
        description: String,
        project: Option<String>,
        priority: i64,
        issue_type: String,
        labels: Vec<String>,
        parent: Option<String>,
    },
    List {
        project: Option<String>,
        status: Option<String>,
        all: bool,
        limit: usize,
        query: Option<String>,
    },
    Ready {
        project: Option<String>,
        limit: usize,
    },
    Show {
        id: String,
    },
    Update {
        id: String,
        title: Option<String>,
        description: Option<String>,
        status: Option<String>,
        priority: Option<i64>,
        note: Option<String>,
        labels_add: Vec<String>,
        labels_remove: Vec<String>,
    },
    Claim {
        id: String,
    },
    Release {
        id: String,
    },
    Close {
        id: String,
        reason: Option<String>,
        pr: Option<String>,
        commit: Option<String>,
        ack: Option<String>,
    },
}

const HELP: &str = "\
**Marbles slash commands**

- `create <title> [--project P] [--description D] [--priority N] [--type T] [--label L] [--parent ID]`
- `list [--project P] [--status S] [--all] [--query TEXT] [--limit N]`
- `ready [--project P] [--limit N]`
- `show <id>`
- `update <id> [--title T] [--description D] [--status S] [--priority N] [--note TEXT] [--label-add L] [--label-remove L]`
- `claim <id>`
- `release <id>`
- `close <id> [--reason TEXT] [--pr URL] [--commit SHA] [--ack TEXT]`

Statuses: open, in_progress, review, blocked, deferred, closed.
`close` requires delivery evidence: a PR, a commit, or an explicit ack.";

fn parse_command(input: &str) -> Result<Command, String> {
    let tokens = tokenize(input)?;
    let Some((verb, rest)) = tokens.split_first() else {
        return Ok(Command::Help);
    };
    match verb.to_ascii_lowercase().as_str() {
        "" | "help" => Ok(Command::Help),
        "create" | "new" => parse_create(rest),
        "list" | "ls" => parse_list(rest),
        "ready" => parse_ready(rest),
        "show" | "get" => parse_show(rest),
        "update" | "edit" => parse_update(rest),
        "claim" => parse_claim(rest),
        "release" => parse_release(rest),
        "close" => parse_close(rest),
        other => Err(format!("unknown command `{other}`. Try `/marbles help`.")),
    }
}

fn tokenize(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut has_token = false;

    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            has_token = true;
            continue;
        }
        match quote {
            Some(delimiter) => {
                if ch == '\\' && delimiter == '"' {
                    escaped = true;
                } else if ch == delimiter {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\\' => escaped = true,
                '"' | '\'' => {
                    quote = Some(ch);
                    has_token = true;
                }
                ch if ch.is_whitespace() => {
                    if has_token {
                        tokens.push(std::mem::take(&mut current));
                        has_token = false;
                    }
                }
                ch => {
                    current.push(ch);
                    has_token = true;
                }
            },
        }
    }

    if escaped {
        return Err("command ends with a trailing backslash".to_string());
    }
    if quote.is_some() {
        return Err("command has an unterminated quote".to_string());
    }
    if has_token {
        tokens.push(current);
    }
    Ok(tokens)
}

fn take_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_number<T: std::str::FromStr>(value: String, flag: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} needs a number, got `{value}`"))
}

fn parse_create(args: &[String]) -> Result<Command, String> {
    let mut index = 0;
    let mut title_words = Vec::new();
    while index < args.len() && !args[index].starts_with('-') {
        title_words.push(args[index].clone());
        index += 1;
    }
    if title_words.is_empty() {
        return Err("create needs a title before any flags".to_string());
    }

    let mut description = String::new();
    let mut project = None;
    let mut priority = 0;
    let mut issue_type = String::new();
    let mut labels = Vec::new();
    let mut parent = None;

    while index < args.len() {
        match args[index].as_str() {
            "--project" | "-p" => project = Some(take_value(args, &mut index, "--project")?),
            "--description" | "--desc" | "-d" => {
                description = take_value(args, &mut index, "--description")?
            }
            "--priority" => {
                priority = parse_number(take_value(args, &mut index, "--priority")?, "--priority")?
            }
            "--type" | "-t" => issue_type = take_value(args, &mut index, "--type")?,
            "--label" | "-l" => labels.push(take_value(args, &mut index, "--label")?),
            "--parent" => parent = Some(take_value(args, &mut index, "--parent")?),
            other => return Err(format!("unknown create flag `{other}`")),
        }
        index += 1;
    }

    Ok(Command::Create {
        title: title_words.join(" "),
        description,
        project,
        priority,
        issue_type,
        labels,
        parent,
    })
}

fn parse_list(args: &[String]) -> Result<Command, String> {
    let mut project = None;
    let mut status = None;
    let mut all = false;
    let mut limit = 20;
    let mut query = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--project" | "-p" => project = Some(take_value(args, &mut index, "--project")?),
            "--status" | "-s" => status = Some(take_value(args, &mut index, "--status")?),
            "--all" | "-a" => all = true,
            "--limit" | "-n" => {
                limit = parse_number(take_value(args, &mut index, "--limit")?, "--limit")?
            }
            "--query" | "-q" => query = Some(take_value(args, &mut index, "--query")?),
            other => return Err(format!("unknown list flag `{other}`")),
        }
        index += 1;
    }
    Ok(Command::List {
        project,
        status,
        all,
        limit,
        query,
    })
}

fn parse_ready(args: &[String]) -> Result<Command, String> {
    let mut project = None;
    let mut limit = 20;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--project" | "-p" => project = Some(take_value(args, &mut index, "--project")?),
            "--limit" | "-n" => {
                limit = parse_number(take_value(args, &mut index, "--limit")?, "--limit")?
            }
            other => return Err(format!("unknown ready flag `{other}`")),
        }
        index += 1;
    }
    Ok(Command::Ready { project, limit })
}

fn parse_show(args: &[String]) -> Result<Command, String> {
    match args {
        [id] => Ok(Command::Show { id: id.clone() }),
        [] => Err("show needs an issue id".to_string()),
        _ => Err("show takes exactly one issue id".to_string()),
    }
}

fn parse_update(args: &[String]) -> Result<Command, String> {
    let Some(id) = args.first().filter(|id| !id.starts_with('-')) else {
        return Err("update needs an issue id".to_string());
    };
    let mut title = None;
    let mut description = None;
    let mut status = None;
    let mut priority = None;
    let mut note = None;
    let mut labels_add = Vec::new();
    let mut labels_remove = Vec::new();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--title" | "-t" => title = Some(take_value(args, &mut index, "--title")?),
            "--description" | "--desc" | "-d" => {
                description = Some(take_value(args, &mut index, "--description")?)
            }
            "--status" | "-s" => status = Some(take_value(args, &mut index, "--status")?),
            "--priority" => {
                priority = Some(parse_number(
                    take_value(args, &mut index, "--priority")?,
                    "--priority",
                )?)
            }
            "--note" | "-n" => note = Some(take_value(args, &mut index, "--note")?),
            "--label-add" => labels_add.push(take_value(args, &mut index, "--label-add")?),
            "--label-remove" => labels_remove.push(take_value(args, &mut index, "--label-remove")?),
            other => return Err(format!("unknown update flag `{other}`")),
        }
        index += 1;
    }
    Ok(Command::Update {
        id: id.clone(),
        title,
        description,
        status,
        priority,
        note,
        labels_add,
        labels_remove,
    })
}

fn parse_claim(args: &[String]) -> Result<Command, String> {
    match args {
        [id] => Ok(Command::Claim { id: id.clone() }),
        [] => Err("claim needs an issue id".to_string()),
        _ => Err("claim takes exactly one issue id; it always claims as you".to_string()),
    }
}

fn parse_release(args: &[String]) -> Result<Command, String> {
    match args {
        [id] => Ok(Command::Release { id: id.clone() }),
        [] => Err("release needs an issue id".to_string()),
        _ => {
            Err("release takes exactly one issue id; it always releases your own claim".to_string())
        }
    }
}

fn parse_close(args: &[String]) -> Result<Command, String> {
    let Some(id) = args.first().filter(|id| !id.starts_with('-')) else {
        return Err("close needs an issue id".to_string());
    };
    let mut reason = None;
    let mut pr = None;
    let mut commit = None;
    let mut ack = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--reason" | "-r" => reason = Some(take_value(args, &mut index, "--reason")?),
            "--pr" => pr = Some(take_value(args, &mut index, "--pr")?),
            "--commit" => commit = Some(take_value(args, &mut index, "--commit")?),
            "--ack" => ack = Some(take_value(args, &mut index, "--ack")?),
            other => return Err(format!("unknown close flag `{other}`")),
        }
        index += 1;
    }
    Ok(Command::Close {
        id: id.clone(),
        reason,
        pr,
        commit,
        ack,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_keeps_quoted_phrases_and_escapes() {
        assert_eq!(
            tokenize("create \"fix the flux capacitor\" --project fpl").unwrap(),
            vec!["create", "fix the flux capacitor", "--project", "fpl"]
        );
        assert_eq!(
            tokenize(r#"create "say \"hello\"" --ack 'nothing shipped'"#).unwrap(),
            vec!["create", "say \"hello\"", "--ack", "nothing shipped"]
        );
        assert!(tokenize("create \"unterminated").is_err());
    }

    #[test]
    fn create_parses_title_and_flags() {
        assert_eq!(
            parse_command(
                "create Fix flux capacitor --project fpl --priority 1 --type bug \
                 --description \"it burns\" --label hardware --parent mb-1"
            )
            .unwrap(),
            Command::Create {
                title: "Fix flux capacitor".into(),
                description: "it burns".into(),
                project: Some("fpl".into()),
                priority: 1,
                issue_type: "bug".into(),
                labels: vec!["hardware".into()],
                parent: Some("mb-1".into()),
            }
        );
    }

    #[test]
    fn list_parses_filters_and_limit() {
        assert_eq!(
            parse_command("ls -p fpl --status blocked --query token --limit 5").unwrap(),
            Command::List {
                project: Some("fpl".into()),
                status: Some("blocked".into()),
                all: false,
                limit: 5,
                query: Some("token".into()),
            }
        );
    }

    #[test]
    fn close_requires_an_id_and_accepts_evidence() {
        assert!(parse_command("close").is_err());
        assert_eq!(
            parse_command("close mb-1 --pr https://example/pr/1 --reason shipped").unwrap(),
            Command::Close {
                id: "mb-1".into(),
                reason: Some("shipped".into()),
                pr: Some("https://example/pr/1".into()),
                commit: None,
                ack: None,
            }
        );
    }

    #[test]
    fn unknown_commands_fail_loudly() {
        let error = parse_command("frobnicate").unwrap_err();
        assert!(error.contains("unknown command `frobnicate`"), "{error}");
    }
}
