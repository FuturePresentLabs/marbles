//! Refresh the `marbles-live` Canvas workspace from the read-only views API.
//!
//! The dashboard -> widget projection lives in `viewer/marbles-view`, compiled
//! to a no-import core wasm module. This binary is only the host: fetch the
//! authenticated dashboard, run the module under wasmtime, then apply the
//! resulting Canvas document through the owner's local `canvasctl`.
//!
//! It never writes tracker state and never invents rows when the API is
//! unavailable. A failed fetch, wasm render, or apply leaves the last good
//! workspace on screen and retries on the interval.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wasmtime::{Engine, Instance, Module, Store};

use marbles::client::{Client, Options};
use marbles::config;

const MAX_VIEW_BYTES: usize = 16 * 1024 * 1024;
const FUEL_BUDGET: u64 = 1_000_000_000;

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
    #[arg(
        long,
        env = "MARBLES_VIEW_WASM",
        default_value = "viewer/marbles-view/target/wasm32-unknown-unknown/release/marbles_view.wasm"
    )]
    wasm: PathBuf,
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

struct ViewModule {
    engine: Engine,
    module: Module,
}

impl ViewModule {
    fn load(path: &Path) -> Result<Self> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|error| format!("wasmtime engine: {error}"))?;
        let module = Module::from_file(&engine, path)
            .map_err(|error| format!("read wasm module {}: {error:#}", path.display()))?;
        Ok(Self { engine, module })
    }

    fn render(&self, dashboard: &Value) -> Result<Value> {
        let input =
            serde_json::to_vec(dashboard).map_err(|error| format!("dashboard JSON: {error}"))?;
        if input.len() > MAX_VIEW_BYTES {
            return Err(format!("dashboard exceeds {MAX_VIEW_BYTES} bytes"));
        }
        let mut store = Store::new(&self.engine, ());
        store
            .set_fuel(FUEL_BUDGET)
            .map_err(|error| format!("wasm fuel: {error}"))?;
        let instance = Instance::new(&mut store, &self.module, &[])
            .map_err(|error| format!("instantiate marbles-view: {error:#}"))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| "marbles-view has no memory export".to_owned())?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|error| format!("marbles-view alloc export: {error}"))?;
        let render = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "render_block")
            .map_err(|error| format!("marbles-view render_block export: {error}"))?;

        let input_ptr = alloc
            .call(&mut store, input.len() as i32)
            .map_err(|error| format!("marbles-view alloc: {error}"))?;
        memory
            .write(&mut store, input_ptr as usize, &input)
            .map_err(|error| format!("marbles-view input write: {error}"))?;
        let result_ptr = render
            .call(&mut store, (input_ptr, input.len() as i32))
            .map_err(|error| format!("marbles-view render: {error:#}"))?;

        let mut header = [0u8; 4];
        memory
            .read(&store, result_ptr as usize, &mut header)
            .map_err(|error| format!("marbles-view result header: {error}"))?;
        let length = u32::from_le_bytes(header) as usize;
        if length > MAX_VIEW_BYTES {
            return Err(format!(
                "marbles-view result exceeds {MAX_VIEW_BYTES} bytes"
            ));
        }
        let mut output = vec![0u8; length];
        memory
            .read(&store, result_ptr as usize + 4, &mut output)
            .map_err(|error| format!("marbles-view result: {error}"))?;
        let value: Value = serde_json::from_slice(&output)
            .map_err(|error| format!("marbles-view result JSON: {error}"))?;
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            return Err(format!("marbles-view rejected dashboard: {error}"));
        }
        Ok(value)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if !args.interval.is_finite() || args.interval <= 0.0 {
        eprintln!("interval must be positive");
        return ExitCode::FAILURE;
    }
    let view = match ViewModule::load(&args.wasm) {
        Ok(view) => view,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let interval = Duration::from_secs_f64(args.interval);
    loop {
        let started = Instant::now();
        match refresh(&args, &view).await {
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

fn number(value: &Value) -> i64 {
    value.as_i64().unwrap_or(0)
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

async fn refresh(args: &Args, view: &ViewModule) -> Result<()> {
    let mut dashboard = fetch_dashboard(args).await?;
    let state = state_path(args);
    let state_revision = read_state_revision(&state).unwrap_or(0);
    let (canvas_revision, runtime_revision) = if args.dry_run {
        (0, 0)
    } else {
        canvas_revisions(args).await?
    };
    let revision = state_revision.max(canvas_revision) + 1;
    dashboard["view_revision"] = json!(revision);
    let mut document = view.render(&dashboard)?;
    if document["revision"].as_u64() != Some(revision) {
        return Err(format!(
            "marbles-view revision mismatch: expected {revision}, got {}",
            document["revision"]
        ));
    }
    document["revision"] = json!(revision);
    write_document(&state, &document)?;
    let totals = &dashboard["totals"];
    log(&format!(
        "rendered total={} ready={} in_progress={} critical={} revision={revision}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_fields_are_read_from_canvasctl_surfaces() {
        let text = "Canvas 1408x881 at 0,0 · 4 surfaces · document rev 17 · runtime rev 19";
        assert_eq!(revision_after(text, "document rev "), Some(17));
        assert_eq!(revision_after(text, "runtime rev "), Some(19));
        assert_eq!(revision_after(text, "missing "), None);
    }
}
