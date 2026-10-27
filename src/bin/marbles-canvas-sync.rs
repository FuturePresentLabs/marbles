//! Refresh the `marbles-live` Canvas workspace from the read-only views API.
//!
//! This is an adapter, not a second source of truth. It reads
//! `views.dashboard`, projects it into Canvas widget JSON, and applies that
//! document through the owner's local `canvasctl`. It never writes tracker
//! state and never invents rows when the API is unavailable. A failed fetch or
//! apply leaves the last good workspace on screen and retries on the interval.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use marbles::client::{Client, Options};
use marbles::config;

const READY_ROWS: usize = 18;
const FLOW_ROWS: usize = 10;
const GANTT_ROWS: usize = 10;

#[derive(Parser)]
#[command(about = "refresh the Marbles Canvas workspace from the read-only views API")]
struct Args {
    #[arg(long, env = "MARBLES_URL", default_value = "http://127.0.0.1:7878")]
    url: String,
    #[arg(long, env = "MARBLES_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long, env = "MARBLES_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    #[arg(long, env = "CANVASCTL", default_value = "canvasctl")]
    canvasctl: PathBuf,
    #[arg(long, default_value_t = 30.0)]
    interval: f64,
    #[arg(long, default_value_t = 20)]
    timeout_seconds: u64,
    #[arg(long)]
    state: Option<PathBuf>,
    #[arg(long)]
    once: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    folder: Option<String>,
    #[arg(long)]
    plan: Option<String>,
    #[arg(long, default_value_t = 14)]
    days: i64,
    #[arg(long, default_value_t = 7)]
    stale_days: i64,
}

type Result<T> = std::result::Result<T, String>;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if !args.interval.is_finite() || args.interval <= 0.0 {
        eprintln!("interval must be positive");
        return ExitCode::FAILURE;
    }
    let interval = Duration::from_secs_f64(args.interval);
    loop {
        let started = Instant::now();
        match refresh(&args).await {
            Ok(()) => {}
            Err(error) => {
                log(&format!("error: {error}"));
                if args.once {
                    return ExitCode::FAILURE;
                }
            }
        }
        if args.once {
            break;
        }
        let wait = interval.saturating_sub(started.elapsed());
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(wait) => {}
        }
    }
    ExitCode::SUCCESS
}

fn log(message: &str) {
    let stamp = chrono::Utc::now().format("%H:%M:%SZ");
    println!("canvas-sync {stamp} {message}");
}

fn state_path(args: &Args) -> PathBuf {
    args.state.clone().unwrap_or_else(|| {
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("marbles")
            .join("canvas-doc.json")
    })
}

fn read_token(args: &Args) -> Result<String> {
    if let Some(token) = &args.token {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(token.to_owned());
        }
    }
    let path = args
        .token_file
        .clone()
        .unwrap_or_else(|| config::token_dir().join("human-avery"));
    let token = fs::read_to_string(&path)
        .map_err(|error| format!("read token file {}: {error}", path.display()))?;
    let token = token.trim();
    if token.is_empty() {
        return Err(format!("token file {} is empty", path.display()));
    }
    Ok(token.to_owned())
}

async fn fetch_dashboard(args: &Args) -> Result<Value> {
    let mut body = json!({"days": args.days, "stale_days": args.stale_days});
    for (key, value) in [
        ("project", args.project.as_deref()),
        ("folder", args.folder.as_deref()),
        ("plan", args.plan.as_deref()),
    ] {
        if let Some(value) = value {
            body[key] = json!(value);
        }
    }
    let client = Client::with_options(
        &args.url,
        &read_token(args)?,
        Options {
            timeout: Some(Duration::from_secs(args.timeout_seconds)),
            cache: None,
        },
    );
    let dashboard: Value = client
        .call("views.dashboard", &body)
        .await
        .map_err(|error| format!("views.dashboard: {error}"))?;
    if dashboard["schema"] != "marbles.view.dashboard.v1" {
        return Err(format!(
            "unexpected dashboard schema: {}",
            dashboard["schema"]
        ));
    }
    Ok(dashboard)
}

fn observed_at(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
        .unwrap_or_else(|| epoch.to_string())
}

fn number(value: &Value) -> i64 {
    value.as_i64().unwrap_or(0)
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn estimate(value: &Value) -> String {
    match value.as_i64() {
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn card_row(card: &Value, status: &str) -> Value {
    json!([
        status,
        text(&card["id"]),
        text(&card["project"]),
        text(&card["title"]),
        text(&card["assignee"]),
        number(&card["blocked_by"]).to_string(),
        estimate(&card["estimate"]),
    ])
}

fn build_document(dashboard: &Value, revision: u64) -> Value {
    let generated_at = number(&dashboard["generated_at"]);
    let observed_at = observed_at(generated_at);
    let data = json!({
        "state": "ready",
        "source": "marbles:views",
        "observed_at": observed_at,
        "window": "now",
        "provenance": "live",
    });
    let totals = &dashboard["totals"];
    let total = number(&totals["total"]);
    let percent_done = totals["percent_done"].as_f64().unwrap_or(0.0);

    let mut ready = Vec::new();
    let mut in_flight = Vec::new();
    if let Some(columns) = dashboard["scrum"]["columns"].as_array() {
        for column in columns {
            let status = text(&column["status"]);
            let cards = column["cards"].as_array().cloned().unwrap_or_default();
            if status == "open" {
                ready = cards
                    .into_iter()
                    .filter(|card| number(&card["blocked_by"]) == 0)
                    .collect();
            } else if status == "in_progress" {
                in_flight = cards;
            }
        }
    }
    let ready_rows: Vec<Value> = ready
        .iter()
        .take(READY_ROWS)
        .map(|card| card_row(card, "open"))
        .collect();
    let flow_rows: Vec<Value> = in_flight
        .iter()
        .take(FLOW_ROWS)
        .map(|card| card_row(card, "in_progress"))
        .collect();

    let nodes = dashboard["gantt"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut by_id: HashMap<String, &Value> = HashMap::new();
    for node in &nodes {
        for key in ["id", "key"] {
            if let Some(id) = node[key].as_str() {
                by_id.insert(id.to_owned(), node);
            }
        }
    }
    let mut gantt_rows = Vec::new();
    if let Some(path) = dashboard["gantt"]["critical_path"].as_array() {
        for id in path.iter().filter_map(Value::as_str).take(GANTT_ROWS) {
            let Some(node) = by_id.get(id) else {
                continue;
            };
            gantt_rows.push(json!([
                format!("{}m", number(&node["start_min"])),
                format!("{}m", number(&node["finish_min"])),
                "critical",
                text(&node["status"]),
                text(&node["key"]),
                text(&node["title"]),
                estimate(&node["estimate"]),
            ]));
        }
    }

    // Canvas retains a live widget when its spec id and family are unchanged,
    // preserving stream-bound values. A snapshot refresh is a new value, so
    // give the retained widget a new id; the surface id and layout stay put.
    let suffix = format!("-{revision}");
    let summary_id = format!("marbles-summary{suffix}");
    let ready_id = format!("marbles-ready{suffix}");
    let flow_id = format!("marbles-flow{suffix}");
    let gantt_id = format!("marbles-gantt{suffix}");

    json!({
        "schema_version": 1,
        "theme": {"source": "project"},
        "background": {"kind": "theme"},
        "chrome": {"title": "hidden", "canvas_padding": 12, "surface_gap": 12},
        "pages": [],
        "active_page": 0,
        "id": "marbles-live",
        "project_id": "marbles",
        "revision": revision,
        "layout": {
            "kind": "canvas",
            "placements": [
                {"surface_id": "marbles-summary", "bounds": {"x": 0.01, "y": 0.01, "width": 0.48, "height": 0.31}, "layer": 0},
                {"surface_id": "marbles-gantt", "bounds": {"x": 0.50, "y": 0.01, "width": 0.49, "height": 0.60}, "layer": 0},
                {"surface_id": "marbles-ready", "bounds": {"x": 0.01, "y": 0.33, "width": 0.48, "height": 0.66}, "layer": 0},
                {"surface_id": "marbles-flow", "bounds": {"x": 0.50, "y": 0.62, "width": 0.49, "height": 0.37}, "layer": 0},
            ],
        },
        "bindings": [],
        "focused_surface_id": "marbles-summary",
        "surfaces": [
            {
                "id": "marbles-summary",
                "title": "Marbles fleet",
                "content": {"kind": "widget", "spec": {
                    "schema_version": 2,
                    "id": summary_id,
                    "kind": "metric_board",
                    "title": "Marbles fleet",
                    "style": "bullet",
                    "metrics": [
                        {"label": "Ready", "value": number(&totals["ready"]), "max": total, "state": "good"},
                        {"label": "In progress", "value": number(&totals["in_progress"]), "max": total},
                        {"label": "Review", "value": number(&totals["review"]), "max": total, "state": "warning"},
                        {"label": "Blocked", "value": number(&totals["blocked"]), "max": total, "state": "critical"},
                        {"label": "Stale", "value": number(&totals["stale"]), "max": total, "state": "warning"},
                        {"label": "Done", "value": percent_done, "max": 100, "target": 100, "state": "neutral"},
                    ],
                    "data": data.clone(),
                }},
                "inputs": [],
            },
            {
                "id": "marbles-ready",
                "title": "Ready work",
                "content": {"kind": "widget", "spec": {
                    "schema_version": 2,
                    "id": ready_id,
                    "kind": "table",
                    "title": "Ready work",
                    "columns": ["Status", "ID", "Project", "Title", "Claim", "Blockers", "Min"],
                    "rows": ready_rows,
                    "data": data.clone(),
                }},
                "inputs": [],
            },
            {
                "id": "marbles-flow",
                "title": "In flight",
                "content": {"kind": "widget", "spec": {
                    "schema_version": 2,
                    "id": flow_id,
                    "kind": "table",
                    "title": "In flight",
                    "columns": ["Status", "ID", "Project", "Title", "Claim", "Blockers", "Min"],
                    "rows": flow_rows,
                    "data": data.clone(),
                }},
                "inputs": [],
            },
            {
                "id": "marbles-gantt",
                "title": "Gantt · critical path",
                "content": {"kind": "widget", "spec": {
                    "schema_version": 2,
                    "id": gantt_id,
                    "kind": "table",
                    "title": "Gantt · critical path",
                    "columns": ["Start", "End", "Path", "Status", "ID", "Title", "Min"],
                    "rows": gantt_rows,
                    "data": data,
                }},
                "inputs": [],
            },
        ],
    })
}

fn read_state_revision(path: &Path) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value["revision"].as_u64()
}

fn revision_after(text: &str, label: &str) -> Option<u64> {
    let start = text.find(label)? + label.len();
    let digits: String = text[start..]
        .chars()
        .skip_while(|character| !character.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

async fn run_canvasctl(args: &Args, input: &str) -> Result<String> {
    let mut child = Command::new(&args.canvasctl)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("canvasctl {}: {error}", args.canvasctl.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "canvasctl stdin unavailable".to_owned())?;
    stdin
        .write_all(input.as_bytes())
        .await
        .map_err(|error| format!("canvasctl stdin: {error}"))?;
    drop(stdin);
    let output = tokio::time::timeout(
        Duration::from_secs(args.timeout_seconds),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| "canvasctl deadline exceeded".to_owned())?
    .map_err(|error| format!("canvasctl wait: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() {
        return Err(format!(
            "canvasctl failed: {}{}{}",
            stdout,
            if stdout.is_empty() || stderr.is_empty() {
                ""
            } else {
                ": "
            },
            stderr
        ));
    }
    if stdout.is_empty() {
        return Err(format!("canvasctl returned no reply: {stderr}"));
    }
    Ok(stdout)
}

async fn canvas_revisions(args: &Args) -> Result<(u64, u64)> {
    let reply = run_canvasctl(args, "surfaces\n").await?;
    let document = revision_after(&reply, "document rev ")
        .ok_or_else(|| format!("canvasctl surfaces omitted document revision: {reply}"))?;
    let runtime = revision_after(&reply, "runtime rev ")
        .ok_or_else(|| format!("canvasctl surfaces omitted runtime revision: {reply}"))?;
    Ok((document, runtime))
}

fn write_document(path: &Path, document: &Value) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("state path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(".canvas-doc-{}.tmp", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_vec(document).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| {
        format!(
            "rename {} -> {}: {error}",
            temporary.display(),
            path.display()
        )
    })
}

async fn apply_document(args: &Args, document: &Value, runtime_revision: u64) -> Result<()> {
    let request = json!({
        "schema_version": 1,
        "request_id": format!("marbles-canvas-{}", now_ms()),
        "expected_revision": runtime_revision,
        "command": {"command": "apply_document", "document": document},
    });
    let input = serde_json::to_string(&request).map_err(|error| error.to_string())?;
    let reply = run_canvasctl(args, &format!("{input}\n")).await?;
    let value: Value = serde_json::from_str(&reply)
        .map_err(|error| format!("canvasctl reply is not JSON: {error}: {reply}"))?;
    let ok = value["outcome"]["status"] == "ok" || value["ok"] == true;
    if !ok {
        return Err(format!("Canvas rejected document: {reply}"));
    }
    let revision = value["revision"].as_u64().unwrap_or(runtime_revision);
    log(&format!(
        "applied document={} revision={revision}",
        document["id"]
    ));
    Ok(())
}

async fn refresh(args: &Args) -> Result<()> {
    let dashboard = fetch_dashboard(args).await?;
    let state = state_path(args);
    let state_revision = read_state_revision(&state).unwrap_or(0);
    let (canvas_revision, runtime_revision) = if args.dry_run {
        (0, 0)
    } else {
        canvas_revisions(args).await?
    };
    let revision = state_revision.max(canvas_revision) + 1;
    let document = build_document(&dashboard, revision);
    write_document(&state, &document)?;
    let totals = &dashboard["totals"];
    log(&format!(
        "fetched total={} ready={} in_progress={} critical={} revision={revision}",
        number(&totals["total"]),
        number(&totals["ready"]),
        number(&totals["in_progress"]),
        dashboard["gantt"]["critical_path"]
            .as_array()
            .map_or(0, Vec::len),
    ));
    if args.dry_run {
        log(&format!("dry-run doc={}", state.display()));
        return Ok(());
    }
    apply_document(args, &document, runtime_revision).await
}
