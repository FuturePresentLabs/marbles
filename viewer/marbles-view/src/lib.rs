//! Marbles view projection.
//!
//! This is the only implementation of the Marbles dashboard -> Canvas widget
//! mapping. It is a pure `serde_json::Value` transform: no HTTP, token, clock,
//! filesystem, environment, or host imports. Native hosts run the compiled
//! module under wasmtime; a browser host runs the same module with
//! `WebAssembly.instantiate`.
//!
//! ABI (core wasm, no imports):
//! - `alloc(len: i32) -> i32`
//! - `render(ptr: i32, len: i32) -> i32`
//!
//! `render` returns a pointer to a result block: 4-byte little-endian length,
//! followed by that many UTF-8 bytes of either the widget document or
//! `{"error":"..."}`. The caller reads the block and drops the instance.

use serde_json::{Value, json};

const READY_ROWS: usize = 18;
const FLOW_ROWS: usize = 10;
const GANTT_ROWS: usize = 10;

/// Project `marbles.view.dashboard.v1` JSON into a Canvas workspace document.
pub fn render(input: &[u8]) -> Result<Vec<u8>, String> {
    let dashboard: Value =
        serde_json::from_slice(input).map_err(|error| format!("dashboard JSON: {error}"))?;
    if dashboard["schema"] != "marbles.view.dashboard.v1" {
        return Err(format!(
            "unexpected dashboard schema: {}",
            dashboard["schema"]
        ));
    }
    let document = project(&dashboard);
    serde_json::to_vec(&document).map_err(|error| format!("widget JSON: {error}"))
}

fn number(value: &Value) -> i64 {
    value.as_i64().unwrap_or(0)
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn estimate(value: &Value) -> String {
    value
        .as_i64()
        .map_or_else(String::new, |value| value.to_string())
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

/// Civil UTC timestamp without a clock crate, so the module stays deterministic.
fn observed_at(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let hour = seconds / 3_600;
    let minute = seconds % 3_600 / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

fn project(dashboard: &Value) -> Value {
    let generated_at = number(&dashboard["generated_at"]);
    let revision = dashboard["view_revision"].as_u64().unwrap_or(1);
    let observed_at = observed_at(generated_at);
    let data = json!({
        "state": "ready",
        "source": "marbles:views",
        "observed_at": observed_at,
        "window": "now",
        "provenance": "live",
    });
    if dashboard["view_mode"] == "burndown" {
        return project_burndown(dashboard, revision, data);
    }

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
    let mut by_id = std::collections::HashMap::new();
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
    // the widget id includes the document revision; the surface id and layout
    // stay stable.
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

fn project_burndown(dashboard: &Value, revision: u64, data: Value) -> Value {
    let mut remaining = Vec::new();
    let mut ideal = Vec::new();
    if let Some(points) = dashboard["burndown"]["points"].as_array() {
        for (index, point) in points.iter().enumerate() {
            let x = index as f64;
            remaining.push(json!({"x": x, "y": number(&point["remaining"]) as f64}));
            ideal.push(json!({"x": x, "y": number(&point["ideal_remaining"]) as f64}));
        }
    }
    let widget_id = format!("marbles-burndown-{revision}");
    json!({
        "schema_version": 1,
        "theme": {"source": "project"},
        "background": {"kind": "theme"},
        "chrome": {"title": "hidden", "canvas_padding": 12, "surface_gap": 12},
        "pages": [],
        "active_page": 0,
        "id": "marbles-burndown",
        "project_id": "marbles",
        "revision": revision,
        "layout": {
            "kind": "canvas",
            "placements": [
                {"surface_id": "marbles-burndown", "bounds": {"x": 0.02, "y": 0.02, "width": 0.96, "height": 0.96}, "layer": 0}
            ],
        },
        "bindings": [],
        "focused_surface_id": null,
        "surfaces": [{
            "id": "marbles-burndown",
            "title": "Marbles burndown",
            "content": {"kind": "widget", "spec": {
                "schema_version": 2,
                "id": widget_id,
                "kind": "time_series",
                "title": "Marbles burndown",
                "unit": "issues",
                // Canvas on FireTV derives its headline value from the last
                // series. Keep Remaining last so the headline is the actual
                // remaining count, not the ideal line's zero endpoint.
                "series": [
                    {"label": "Ideal", "points": ideal},
                    {"label": "Remaining", "points": remaining},
                ],
                "data": data,
            }},
            "inputs": [],
        }],
    })
}

/// Reserve `len` bytes for the host to write input JSON into.
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: i32) -> i32 {
    if len < 0 {
        return 0;
    }
    let mut buffer = Vec::<u8>::with_capacity(len as usize);
    let pointer = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    pointer as i32
}

/// Render a dashboard JSON block and return a length-prefixed result block.
///
/// # Safety
///
/// `ptr` must point to `len` initialized bytes in this module's linear memory,
/// as written by the host after calling [`alloc`]. The returned pointer is only
/// valid until the instance is dropped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn render_block(ptr: i32, len: i32) -> i32 {
    let input = if ptr == 0 || len < 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }
    };
    let output = render(input).unwrap_or_else(|error| {
        serde_json::to_vec(&json!({"error": error}))
            .unwrap_or_else(|_| b"{\"error\":\"render failed\"}".to_vec())
    });
    let mut block = Vec::with_capacity(output.len() + 4);
    block.extend_from_slice(&(output.len() as u32).to_le_bytes());
    block.extend_from_slice(&output);
    let pointer = block.as_ptr();
    std::mem::forget(block);
    pointer as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dashboard() -> Value {
        json!({
            "schema": "marbles.view.dashboard.v1",
            "generated_at": 0,
            "totals": {
                "total": 3,
                "ready": 1,
                "in_progress": 1,
                "review": 0,
                "blocked": 1,
                "stale": 0,
                "percent_done": 33.3
            },
            "scrum": {"columns": [
                {"status": "open", "cards": [
                    {"id": "ready", "project": "demo", "title": "Ready", "blocked_by": 0},
                    {"id": "blocked", "project": "demo", "title": "Blocked", "blocked_by": 2}
                ]},
                {"status": "in_progress", "cards": [
                    {"id": "running", "project": "demo", "title": "Running", "blocked_by": 0}
                ]}
            ]},
            "gantt": {
                "critical_path": ["ready"],
                "nodes": [{
                    "id": "ready",
                    "key": "ready",
                    "title": "Ready",
                    "status": "open",
                    "start_min": 0,
                    "finish_min": 30,
                    "estimate": 30
                }]
            }
        })
    }

    #[test]
    fn ready_filters_blocked_cards_and_projects_gantt() {
        let output = render(&serde_json::to_vec(&dashboard()).unwrap()).unwrap();
        let document: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            document["surfaces"][0]["content"]["spec"]["data"]["observed_at"],
            "1970-01-01T00:00:00+00:00"
        );
        let ready = &document["surfaces"][1]["content"]["spec"]["rows"];
        assert_eq!(ready.as_array().map(Vec::len), Some(1));
        assert_eq!(ready[0][1], "ready");
        let flow = &document["surfaces"][2]["content"]["spec"]["rows"];
        assert_eq!(flow[0][1], "running");
        let gantt = &document["surfaces"][3]["content"]["spec"]["rows"];
        assert_eq!(gantt[0][0], "0m");
        assert_eq!(gantt[0][2], "critical");
    }

    #[test]
    fn invalid_schema_is_an_error_not_a_zero_dashboard() {
        let error = render(br#"{"schema":"wrong"}"#).unwrap_err();
        assert!(error.contains("unexpected dashboard schema"), "{error}");
    }

    #[test]
    fn burndown_mode_returns_one_time_series_surface() {
        let mut dashboard = dashboard();
        dashboard["view_mode"] = json!("burndown");
        dashboard["burndown"] = json!({
            "points": [
                {"remaining": 12, "ideal_remaining": 12},
                {"remaining": 8, "ideal_remaining": 6},
                {"remaining": 4, "ideal_remaining": 0}
            ]
        });
        let output = render(&serde_json::to_vec(&dashboard).unwrap()).unwrap();
        let document: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(document["id"], "marbles-burndown");
        assert_eq!(document["surfaces"].as_array().map(Vec::len), Some(1));
        let spec = &document["surfaces"][0]["content"]["spec"];
        assert_eq!(spec["kind"], "time_series");
        assert_eq!(spec["series"][0]["label"], "Ideal");
        assert_eq!(spec["series"][0]["points"][2]["y"], 0.0);
        assert_eq!(spec["series"][1]["label"], "Remaining");
        assert_eq!(spec["series"][1]["points"][2]["y"], 4.0);
    }
}
