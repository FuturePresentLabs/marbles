//! The records-backed command runner.
//!
//! `bin/mb.rs` is a thin wrapper: it parses with [`crate::records_cli`], opens the
//! host's book through [`Adapter`], and prints what this module returns. Keeping
//! the translation here — over the [`Store`] contract, not over SQLite — means
//! the whole command surface is testable on the host against
//! `Adapter::in_memory()`, even though the shipped binary is wasm.
//!
//! Every answer is JSON. The native CLI has a human-readable mode; a Spore is an
//! interface for agents and for the shim, and one output shape is easier to keep
//! identical to the native `--json` contract.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Value, json};

use crate::adapter::Adapter;
use crate::capability;
use crate::plan;
use crate::records_cli::{
    CapabilityAction, CreateArgs, FolderAction, GoalAction, Invocation, PlanAction, Request,
    UpdateArgs, WorkCreateArgs,
};
use crate::report;
use crate::store::Store;
use crate::types::{ActorKind, ClaimRequest, Evidence, Folder, IssuePatch, NewIssue};

pub fn execute(store: &Adapter, invocation: &Invocation, now: i64) -> Result<Value, String> {
    let actor = invocation.actor.as_str();
    let project = invocation.project.as_deref();
    match &invocation.request {
        Request::List { all, id, limit } => {
            let mut issues = store.list(project, *all, id.as_deref()).map_err(err)?;
            if *limit > 0 {
                issues.truncate(*limit as usize);
            }
            value(&issues)
        }
        Request::Ready => value(&store.ready(project, now).map_err(err)?),
        // A one-element array: the native CLI prints this shape because Beads
        // clients parse `show --json` as a list, and `soil-marbles` reads it that
        // way too. The records backend answers the same contract.
        Request::Show { id } => value(&[store.get(id).map_err(err)?]),
        Request::Create(args) => {
            let slug = require_project(project, "create")?;
            ensure_project(store, slug, invocation.prefix.as_deref())?;
            let id = store
                .create(&new_issue(args, slug, actor)?, now)
                .map_err(err)?;
            Ok(json!({ "id": id }))
        }
        Request::Update(args) => update(store, args, actor, now),
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
                        assignee: assignee.clone().unwrap_or_else(|| actor.to_string()),
                        actor_kind: *kind,
                        ttl_seconds: *ttl_seconds,
                    },
                    now,
                )
                .map_err(err)?;
            value(&receipt)
        }
        Request::Touch { id, holder } => {
            let expires_at = store
                .touch(id, holder.as_deref().unwrap_or(actor), now)
                .map_err(err)?;
            Ok(json!({ "expires_at": expires_at }))
        }
        Request::Release { id, holder } => {
            store
                .release(id, holder.as_deref().unwrap_or(actor), now)
                .map_err(err)?;
            Ok(json!({ "released": id }))
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
            value(&issue)
        }
        Request::Close {
            id,
            reason,
            evidence,
        } => {
            let issue = store
                .close(id, reason.as_deref(), evidence, actor, now)
                .map_err(err)?;
            value(&issue)
        }
        Request::Supersede { id, with } => {
            store.supersede(id, with, actor, now).map_err(err)?;
            Ok(json!({ "superseded": id, "by": with }))
        }
        Request::Dep { add, child, parent } => {
            if *add {
                store.add_dep(child, parent, actor, now).map_err(err)?;
            } else {
                store.remove_dep(child, parent, actor, now).map_err(err)?;
            }
            Ok(json!({ "child": child, "parent": parent, "present": add }))
        }
        Request::Label { add, id, label } => {
            if *add {
                store.add_label(id, label, actor, now).map_err(err)?;
            } else {
                store.remove_label(id, label, actor, now).map_err(err)?;
            }
            Ok(json!({ "id": id, "label": label, "present": add }))
        }
        Request::History { id } => value(&store.history(id).map_err(err)?),
        Request::Sweep => value(&store.sweep(now).map_err(err)?),
        Request::Stats => store.stats().map_err(err),
        Request::Projects => projects(store),
        Request::ProjectRemove { slug } => {
            let report = store.drop_project(slug).map_err(err)?;
            Ok(json!({ "removed": slug, "issues": report.issues }))
        }
        Request::Folder(action) => folder(store, action, now),
        Request::Goal(action) => goal(
            store,
            action,
            project,
            invocation.prefix.as_deref(),
            actor,
            now,
        ),
        Request::Epic(action) => epic(store, action, actor, now),
        Request::Task(action) => task(store, action, actor, now),
        Request::Burndown { goal, epic, days } => {
            burndown(store, goal.as_deref(), epic.as_deref(), *days, project, now)
        }
        Request::Portfolio {
            folder,
            days,
            stale_days,
        } => portfolio(store, folder.as_deref(), *days, *stale_days, now),
        Request::Plan(action) => plan_command(
            store,
            action,
            project,
            invocation.prefix.as_deref(),
            actor,
            now,
        ),
        Request::Capability(action) => capability_command(store, action, now),
        Request::Inventory { max_age_days } => {
            if *max_age_days < 0 {
                return Err("--max-age-days cannot be negative".into());
            }
            let max_age = max_age_days * 24 * 60 * 60;
            let capabilities = capability::views(store, now, max_age);
            let ready = store.ready(project, now).map_err(err)?;
            let decisions = capability::decisions(store);
            let measurements = capability::baseline(store, None);
            Ok(json!({
                "generated_at": now,
                "max_age_seconds": max_age,
                "scope": project,
                "counts": {
                    "capabilities": capabilities.len(),
                    "ready_tasks": ready.len(),
                    "decisions": decisions.len(),
                    "measurements": measurements.len(),
                },
                "capabilities": capabilities,
                "ready_tasks": ready,
                "decisions": decisions,
                "measurements": measurements,
            }))
        }
        Request::Doctor => doctor(store),
    }
}

fn capability_command(
    store: &Adapter,
    action: &CapabilityAction,
    now: i64,
) -> Result<Value, String> {
    let age = |days: i64| -> Result<i64, String> {
        if days < 0 {
            return Err("--max-age-days cannot be negative".into());
        }
        Ok(days * 24 * 60 * 60)
    };
    match action {
        CapabilityAction::Create { id, patch } => {
            let capability = capability::create(store, id, patch, now)?;
            serde_json::to_value(capability).map_err(|e| e.to_string())
        }
        CapabilityAction::Update { id, patch } => {
            let capability = capability::update(store, id, patch, now)?;
            serde_json::to_value(capability).map_err(|e| e.to_string())
        }
        CapabilityAction::List {
            implementation,
            health,
            commercial_readiness,
            missing_layer,
            stale,
            max_age_days,
        } => {
            let max_age = age(*max_age_days)?;
            let views: Vec<capability::CapabilityView> = capability::views(store, now, max_age)
                .into_iter()
                .filter(|v| {
                    implementation
                        .as_deref()
                        .is_none_or(|wanted| v.implementation == wanted)
                })
                .filter(|v| health.as_deref().is_none_or(|wanted| v.health == wanted))
                .filter(|v| {
                    commercial_readiness
                        .as_deref()
                        .is_none_or(|wanted| v.commercial_readiness == wanted)
                })
                .filter(|v| {
                    missing_layer
                        .as_deref()
                        .is_none_or(|layer| v.gaps.iter().any(|gap| gap == layer))
                })
                .filter(|v| !*stale || !v.stale.is_empty())
                .collect();
            value(&views)
        }
        CapabilityAction::Show { id, max_age_days } => {
            capability::show(store, id, now, age(*max_age_days)?)
        }
        CapabilityAction::Verify(spec) => {
            let verification = capability::verify(store, spec, now)?;
            serde_json::to_value(verification).map_err(|e| e.to_string())
        }
        CapabilityAction::Measure(spec) => {
            let measurement = capability::measure(store, spec, now)?;
            serde_json::to_value(measurement).map_err(|e| e.to_string())
        }
        CapabilityAction::Decide(spec) => {
            let decision = capability::decide(store, spec, now)?;
            serde_json::to_value(decision).map_err(|e| e.to_string())
        }
        CapabilityAction::Verified {
            layer,
            max_age_days,
        } => value(&capability::verified(
            store,
            layer.as_deref(),
            now,
            age(*max_age_days)?,
        )?),
        CapabilityAction::Gaps {
            layer,
            max_age_days,
        } => value(&capability::gaps(store, layer, now, age(*max_age_days)?)?),
        CapabilityAction::Stale { max_age_days } => {
            value(&capability::stale(store, now, age(*max_age_days)?))
        }
        CapabilityAction::PilotBlockers { max_age_days } => {
            value(&capability::pilot_blockers(store, now, age(*max_age_days)?))
        }
        CapabilityAction::Baseline { metric } => {
            value(&capability::baseline(store, metric.as_deref()))
        }
    }
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

fn update(store: &Adapter, args: &UpdateArgs, actor: &str, now: i64) -> Result<Value, String> {
    let metadata = match (
        metadata_object(args.metadata.as_deref())?,
        &args.set_metadata,
    ) {
        (base, Some((key, value))) => {
            let mut base = base.unwrap_or_else(|| json!({}));
            base.as_object_mut()
                .expect("metadata_object built an object")
                .insert(key.clone(), json!(value));
            Some(base)
        }
        (base, None) => base,
    };
    // Assignment is a claim, not a metadata edit — the same rule as the native
    // CLI. The Spore has no `$USER`, so an explicit `--assignee` claims for an
    // agent; a human claim still says `claim --human`.
    if let Some(assignee) = &args.assignee {
        store
            .claim(
                &ClaimRequest {
                    id: args.id.clone(),
                    assignee: assignee.clone(),
                    actor_kind: ActorKind::Agent,
                    ttl_seconds: None,
                },
                now,
            )
            .map_err(err)?;
    }
    let issue = store
        .update(
            &args.id,
            &IssuePatch {
                title: args.title.clone(),
                description: match &args.description {
                    Some(text) => Some(text_arg("--description", text)?),
                    None => None,
                },
                status: args.status.clone(),
                priority: args.priority,
                append_notes: args.append_notes.clone(),
                available_at: None,
                evidence: None,
                metadata,
            },
            actor,
            now,
        )
        .map_err(err)?;
    value(&issue)
}

fn new_issue(args: &CreateArgs, project: &str, actor: &str) -> Result<NewIssue, String> {
    Ok(NewIssue {
        title: args.title.clone(),
        description: text_arg("--description", &args.description)?,
        issue_type: if args.issue_type.is_empty() {
            "task".into()
        } else {
            args.issue_type.clone()
        },
        priority: args.priority,
        labels: args.labels.clone(),
        parent: args.parent.clone(),
        id: args.id.clone(),
        project: Some(project.to_string()),
        available_at: args.available_at,
        created_by: Some(actor.to_string()),
        metadata: Some(create_metadata(args)?),
        external_ref: args.external_ref.clone(),
    })
}

/// The metadata block for `create`: an optional JSON object plus the same
/// plan-node sugar the native CLI accepts as individual flags.
fn create_metadata(args: &CreateArgs) -> Result<Value, String> {
    let mut metadata = metadata_object(args.metadata.as_deref())?.unwrap_or_else(|| json!({}));
    let map = metadata
        .as_object_mut()
        .expect("metadata_object built an object");
    let sugar = [
        ("plan", args.plan.as_ref().map(|v| json!(v))),
        ("done_means", args.done_means.as_ref().map(|v| json!(v))),
        ("repo", args.repo.as_ref().map(|v| json!(v))),
        (
            "conflict_group",
            args.conflict_group.as_ref().map(|v| json!(v)),
        ),
        ("risk", args.risk.as_ref().map(|v| json!(v))),
        ("acceptance", args.acceptance.as_ref().map(|v| json!(v))),
        ("estimate", args.estimate.map(|v| json!(v))),
    ];
    for (key, value) in sugar {
        if let Some(value) = value {
            map.insert(key.to_string(), value);
        }
    }
    Ok(metadata)
}

fn goal(
    store: &Adapter,
    action: &GoalAction,
    project: Option<&str>,
    prefix: Option<&str>,
    actor: &str,
    now: i64,
) -> Result<Value, String> {
    match action {
        GoalAction::Create(args) => {
            let slug = require_project(project, "goal create")?;
            ensure_project(store, slug, prefix)?;
            let id = create_work(store, args, slug, "goal", None, actor, now)?;
            Ok(json!({ "id": id }))
        }
        GoalAction::List { all } => {
            let issues = store.list(project, true, None).map_err(err)?;
            let mut goals = report::goal_summaries(&issues, project);
            if !*all {
                goals.retain(|goal| goal.status != "closed");
            }
            value(&goals)
        }
        GoalAction::Show { id } => {
            let root = store.get(id).map_err(err)?;
            let issues = store.list(Some(&root.project), true, None).map_err(err)?;
            Ok(json!({
                "issue": root,
                "progress": report::progress_for(&issues, &root.id),
                "tree": report::tree_lines(&issues, &root.id),
            }))
        }
    }
}

fn epic(
    store: &Adapter,
    action: &crate::records_cli::EpicAction,
    actor: &str,
    now: i64,
) -> Result<Value, String> {
    let crate::records_cli::EpicAction::Create { goal, args } = action;
    let parent = store.get(goal).map_err(err)?;
    if parent.issue_type != "goal" {
        return Err(format!("{goal} is a {}, not a goal", parent.issue_type));
    }
    let id = create_work(
        store,
        args,
        &parent.project,
        "epic",
        Some(&parent.id),
        actor,
        now,
    )?;
    Ok(json!({ "id": id }))
}

fn task(
    store: &Adapter,
    action: &crate::records_cli::TaskAction,
    actor: &str,
    now: i64,
) -> Result<Value, String> {
    let crate::records_cli::TaskAction::Create { epic, args } = action;
    let parent = store.get(epic).map_err(err)?;
    if parent.issue_type != "epic" {
        return Err(format!("{epic} is a {}, not an epic", parent.issue_type));
    }
    let id = create_work(
        store,
        args,
        &parent.project,
        "task",
        Some(&parent.id),
        actor,
        now,
    )?;
    Ok(json!({ "id": id }))
}

fn create_work(
    store: &Adapter,
    args: &WorkCreateArgs,
    project: &str,
    issue_type: &str,
    parent: Option<&str>,
    actor: &str,
    now: i64,
) -> Result<String, String> {
    store
        .create(
            &NewIssue {
                title: args.title.clone(),
                description: text_arg("--description", &args.description)?,
                issue_type: issue_type.into(),
                priority: args.priority,
                labels: args.labels.clone(),
                parent: parent.map(str::to_string),
                id: None,
                project: Some(project.to_string()),
                available_at: args.available_at,
                created_by: Some(actor.to_string()),
                metadata: None,
                external_ref: None,
            },
            now,
        )
        .map_err(err)
}

// ---------------------------------------------------------------------------
// Folders, projects, portfolio
// ---------------------------------------------------------------------------

fn folder(store: &Adapter, action: &FolderAction, now: i64) -> Result<Value, String> {
    match action {
        FolderAction::Create { name, slug, parent } => {
            let slug = slug.clone().unwrap_or_else(|| slugify(name));
            if slug.is_empty() {
                return Err("folder name produced an empty slug".into());
            }
            store
                .ensure_folder(
                    &Folder {
                        slug: slug.clone(),
                        name: name.clone(),
                        parent: parent.clone(),
                        created_at: now,
                    },
                    now,
                )
                .map_err(err)?;
            Ok(json!({ "slug": slug }))
        }
        // `list` and `tree` are the same view: the folders, and their parents.
        // A caller that wants a tree renders it from `parent`; the Spore does
        // not own a terminal.
        FolderAction::List | FolderAction::Tree => value(&store.folder_view().map_err(err)?),
        FolderAction::Move { project, folder } => {
            store
                .set_project_folder(project, folder.as_deref(), now)
                .map_err(err)?;
            Ok(json!({ "project": project, "folder": folder }))
        }
    }
}

fn projects(store: &Adapter) -> Result<Value, String> {
    let view = store.folder_view().map_err(err)?;
    let rows = store
        .projects()
        .map_err(err)?
        .into_iter()
        .map(|(slug, root, prefix)| {
            let folder = view
                .projects
                .iter()
                .find(|entry| entry.project == slug)
                .map(|entry| entry.folder.clone());
            json!({ "slug": slug, "root": root, "prefix": prefix, "folder": folder })
        })
        .collect::<Vec<_>>();
    Ok(Value::Array(rows))
}

fn portfolio(
    store: &Adapter,
    folder: Option<&str>,
    days: i64,
    stale_days: i64,
    now: i64,
) -> Result<Value, String> {
    let view = store.folder_view().map_err(err)?;
    let mut projects = store
        .projects()
        .map_err(err)?
        .into_iter()
        .map(|(slug, root, prefix)| report::ProjectInfo {
            folder: view
                .projects
                .iter()
                .find(|entry| entry.project == slug)
                .map(|entry| entry.folder.clone()),
            slug,
            root,
            prefix,
        })
        .collect::<Vec<_>>();
    if let Some(folder) = folder {
        projects.retain(|project| project.folder.as_deref() == Some(folder));
    }
    let issues = store.list(None, true, None).map_err(err)?;
    value(&report::portfolio(
        &projects,
        &view.folders,
        &issues,
        now,
        days,
        stale_days,
    ))
}

fn burndown(
    store: &Adapter,
    goal: Option<&str>,
    epic: Option<&str>,
    days: i64,
    project: Option<&str>,
    now: i64,
) -> Result<Value, String> {
    let root = goal.or(epic);
    let issues = if let Some(root_id) = root {
        let root_issue = store.get(root_id).map_err(err)?;
        let all = store
            .list(Some(&root_issue.project), true, None)
            .map_err(err)?;
        let ids = report::descendant_ids(&all, root_id);
        all.into_iter()
            .filter(|issue| issue.id != *root_id && ids.contains(&issue.id))
            .collect()
    } else {
        store.list(project, true, None).map_err(err)?
    };
    value(&report::burndown(&issues, days, now))
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

fn plan_command(
    store: &Adapter,
    action: &PlanAction,
    project: Option<&str>,
    prefix: Option<&str>,
    actor: &str,
    now: i64,
) -> Result<Value, String> {
    match action {
        PlanAction::Show { plan } => {
            let graph = plan::load(store, plan).map_err(err)?;
            value(&graph.view().map_err(err)?)
        }
        PlanAction::Gantt { plan } => {
            let graph = plan::load(store, plan).map_err(err)?;
            value(&plan::gantt(&graph, now).map_err(err)?)
        }
        PlanAction::Ready { plan } => {
            let graph = plan::load(store, plan).map_err(err)?;
            value(&plan::ready(&graph, now).map_err(err)?)
        }
        PlanAction::Create {
            spec,
            plan,
            project: flag_project,
            dry_run,
        } => {
            let text = spec_text(spec)?;
            let spec = plan::parse_spec(&text)?;
            let project = flag_project
                .clone()
                .or_else(|| spec.project.clone())
                .or_else(|| project.map(str::to_string))
                .filter(|project| !project.is_empty())
                .ok_or("plan create needs a project: pass --project SLUG or set MARBLES_PROJECT")?;
            let resolved = plan::ResolvedPlan {
                plan: plan
                    .clone()
                    .or_else(|| spec.name.clone())
                    .unwrap_or_else(|| "plan".to_string()),
                project: project.clone(),
                nodes: spec.nodes,
            };
            if *dry_run {
                return value(&plan::draft(&resolved).map_err(|problems| problems.join("; "))?);
            }
            ensure_project(store, &project, prefix)?;
            value(&plan::create(store, &resolved, now, actor)?)
        }
    }
}

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

/// A real integrity read, not a reassuring constant: every live issue must name
/// a project that exists, and every local dependency edge must point at an issue
/// that exists. A dangling edge is reported, never silently dropped.
fn doctor(store: &Adapter) -> Result<Value, String> {
    let issues = store.list(None, true, None).map_err(err)?;
    let projects = store.projects().map_err(err)?;
    let view = store.folder_view().map_err(err)?;
    let project_slugs: BTreeSet<&str> = projects.iter().map(|(slug, _, _)| slug.as_str()).collect();
    let ids: BTreeSet<&str> = issues.iter().map(|issue| issue.id.as_str()).collect();
    let mut problems = Vec::new();
    let mut dependencies = 0usize;
    for issue in &issues {
        if !project_slugs.contains(issue.project.as_str()) {
            problems.push(format!(
                "issue {} names missing project {:?}",
                issue.id, issue.project
            ));
        }
        for dep in &issue.dependencies {
            dependencies += 1;
            if let Some(parent) = dep.depends_on_id.as_deref()
                && !ids.contains(parent)
            {
                problems.push(format!("issue {} depends on missing {}", issue.id, parent));
            }
        }
    }
    let ok = problems.is_empty();
    Ok(json!({
        "backend": "soil:records",
        "store_id": store.store_id(),
        "integrity": if ok { "ok" } else { "problems" },
        "quick_check": if ok { "ok" } else { "failed" },
        "projects": projects.len(),
        "issues": issues.len(),
        "dependencies": dependencies,
        "folders": view.folders.len(),
        "problems": problems,
    }))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

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

fn require_project<'a>(project: Option<&'a str>, command: &str) -> Result<&'a str, String> {
    project.ok_or_else(|| {
        format!("{command} needs a project: pass --project SLUG or set MARBLES_PROJECT")
    })
}

/// Parse a metadata argument. Inline JSON is the portable form; `-` reads
/// stdin. `@file` is refused by name because the host owns the filesystem this
/// command is not given.
fn metadata_object(text: Option<&str>) -> Result<Option<Value>, String> {
    let Some(text) = text else {
        return Ok(None);
    };
    if text.starts_with('@') {
        return Err(
            "--metadata @file cannot be read: this build has no filesystem. Pass inline JSON, \
             or `-` to read stdin"
                .into(),
        );
    }
    let text = if text == "-" {
        read_stdin()?
    } else {
        text.to_string()
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("metadata must be a JSON object: {error}"))?;
    if !value.is_object() {
        return Err("metadata must be a JSON object".into());
    }
    Ok(Some(value))
}

/// `-` reads stdin; `@path` is refused; anything else is the literal text.
fn text_arg(flag: &str, text: &str) -> Result<String, String> {
    if text.starts_with('@') {
        return Err(format!(
            "{flag} @file cannot be read: this build has no filesystem. Pass the text inline, \
             or `-` to read stdin"
        ));
    }
    if text == "-" {
        return read_stdin();
    }
    Ok(text.to_string())
}

/// A plan spec is inline TOML/JSON, or `-` for stdin. A bare path is refused
/// rather than parsed as TOML, which would fail with a misleading syntax error.
fn spec_text(spec: &str) -> Result<String, String> {
    if spec == "-" {
        return read_stdin();
    }
    if spec.starts_with('@') {
        return Err(
            "plan create @file cannot be read: this build has no filesystem. Pass the spec \
             inline, or `-` to read stdin"
                .into(),
        );
    }
    let trimmed = spec.trim_start();
    if trimmed.starts_with('{') || trimmed.contains('=') || trimmed.contains('\n') {
        return Ok(spec.to_string());
    }
    Err(format!(
        "plan create wants the spec itself, not a path: {spec:?}. Pass inline TOML/JSON, or `-` \
         to read stdin"
    ))
}

fn read_stdin() -> Result<String, String> {
    use std::io::Read;
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| format!("reading stdin: {error}"))?;
    Ok(text)
}

/// Mirrors `config::slugify` (which is host-only) so the Spore files the same
/// folder slug a native CLI would.
fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.extend(ch.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn err(error: crate::store::Error) -> String {
    error.to_string()
}

fn value<T: Serialize + ?Sized>(value: &T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}
