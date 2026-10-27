#!/usr/bin/env python3
"""Refresh the Marbles Canvas workspace from the read-only views API.

This is an adapter, not a second source of truth: it reads
`views.dashboard` and projects the result into the Canvas widget document.
It never writes tracker state and never invents rows when the API is
unavailable. A transient fetch/apply failure leaves the last good Canvas
workspace on screen and retries on the next interval.

The token is read from a private file on every cycle and is never written to
the Canvas document or logs.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

DEFAULT_URL = "http://127.0.0.1:7878"
DEFAULT_TOKEN_FILE = Path("~/.marbles/tokens/human-avery")
DEFAULT_INTERVAL = 30.0
DEFAULT_TIMEOUT = 20.0

READY_ROWS = 18
FLOW_ROWS = 10
GANTT_ROWS = 10

STOP = False


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Never forward the bearer token to a redirect target."""

    def redirect_request(self, *_args: Any, **_kwargs: Any) -> None:
        return None


OPENER = urllib.request.build_opener(NoRedirect)


def log(message: str) -> None:
    stamp = datetime.now(timezone.utc).strftime("%H:%M:%SZ")
    print(f"canvas-sync {stamp} {message}", flush=True)


def stop(_signum: int, _frame: Any) -> None:
    global STOP
    STOP = True


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default=os.environ.get("MARBLES_URL", DEFAULT_URL))
    parser.add_argument(
        "--token-file",
        type=Path,
        default=Path(os.environ.get("MARBLES_TOKEN_FILE", DEFAULT_TOKEN_FILE)).expanduser(),
    )
    parser.add_argument(
        "--canvasctl",
        default=os.environ.get("CANVASCTL", "canvasctl"),
        help="canvasctl executable (default: $CANVASCTL or PATH)",
    )
    parser.add_argument(
        "--state",
        type=Path,
        default=Path(
            os.environ.get(
                "XDG_CACHE_HOME",
                Path("~/.cache").expanduser(),
            )
        )
        / "marbles"
        / "canvas-doc.json",
    )
    parser.add_argument("--interval", type=float, default=DEFAULT_INTERVAL)
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT)
    parser.add_argument("--once", action="store_true", help="refresh once and exit")
    parser.add_argument("--dry-run", action="store_true", help="write the doc but do not apply it")
    parser.add_argument("--project", help="optional ViewScope project filter")
    parser.add_argument("--folder", help="optional ViewScope folder filter")
    parser.add_argument("--plan", help="optional ViewScope plan filter")
    parser.add_argument("--days", type=int, default=14)
    parser.add_argument("--stale-days", type=int, default=7)
    return parser.parse_args()


def read_token(path: Path) -> str:
    try:
        token = path.read_text(encoding="utf-8").strip()
    except OSError as error:
        raise RuntimeError(f"read token file {path}: {error}") from error
    if not token:
        raise RuntimeError(f"token file {path} is empty")
    return token


def fetch_dashboard(args: argparse.Namespace) -> dict[str, Any]:
    body: dict[str, Any] = {
        "days": args.days,
        "stale_days": args.stale_days,
    }
    for key in ("project", "folder", "plan"):
        value = getattr(args, key)
        if value:
            body[key] = value
    request = urllib.request.Request(
        f"{args.url.rstrip('/')}/v1/views.dashboard",
        data=json.dumps(body).encode("utf-8"),
        headers={
            "authorization": f"Bearer {read_token(args.token_file)}",
            "content-type": "application/json",
            "accept": "application/json",
        },
        method="POST",
    )
    try:
        with OPENER.open(request, timeout=args.timeout) as response:
            payload = json.load(response)
    except urllib.error.HTTPError as error:
        detail = error.read(500).decode("utf-8", "replace").strip()
        raise RuntimeError(f"views.dashboard HTTP {error.code}: {detail}") from error
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
        raise RuntimeError(f"views.dashboard: {error}") from error
    if payload.get("schema") != "marbles.view.dashboard.v1":
        raise RuntimeError(f"unexpected dashboard schema: {payload.get('schema')!r}")
    return payload


def observed_at(epoch: int | float) -> str:
    return datetime.fromtimestamp(float(epoch), tz=timezone.utc).isoformat(timespec="seconds")


def estimate(value: Any) -> str:
    return "" if value is None else str(value)


def table_data(generated_at: int | float) -> dict[str, Any]:
    return {
        "state": "ready",
        "source": "marbles:views",
        "observed_at": observed_at(generated_at),
        "window": "now",
        "provenance": "live",
    }


def card_row(card: dict[str, Any], status: str) -> list[str]:
    return [
        status,
        str(card.get("id", "")),
        str(card.get("project", "")),
        str(card.get("title", "")),
        str(card.get("assignee") or ""),
        str(card.get("blocked_by", 0)),
        estimate(card.get("estimate")),
    ]


def build_document(dashboard: dict[str, Any], revision: int) -> dict[str, Any]:
    generated_at = dashboard.get("generated_at", time.time())
    totals = dashboard.get("totals", {})
    scrum = dashboard.get("scrum", {})
    gantt = dashboard.get("gantt", {})
    columns = scrum.get("columns", [])

    open_cards: list[dict[str, Any]] = []
    in_flight: list[dict[str, Any]] = []
    for column in columns:
        status = column.get("status")
        cards = column.get("cards", [])
        if status == "open":
            open_cards = [card for card in cards if int(card.get("blocked_by") or 0) == 0]
        elif status == "in_progress":
            in_flight = cards

    nodes = gantt.get("nodes", [])
    by_id = {str(node.get("id") or node.get("key")): node for node in nodes}
    critical_rows = []
    for node_id in gantt.get("critical_path", []):
        node = by_id.get(str(node_id))
        if node is None:
            continue
        critical_rows.append(
            [
                f"{node.get('start_min', 0)}m",
                f"{node.get('finish_min', 0)}m",
                "critical",
                str(node.get("status", "")),
                str(node.get("key") or node.get("id", "")),
                str(node.get("title", "")),
                estimate(node.get("estimate")),
            ]
        )

    total = int(totals.get("total") or 0)
    percent_done = float(totals.get("percent_done") or 0.0)
    data = table_data(generated_at)
    # Canvas keeps a live widget when its spec id and family are unchanged,
    # preserving stream-bound values. A snapshot refresh is a new value, so
    # give the retained widget a new id; the surface id (and layout) stays put.
    suffix = f"-{revision}"

    return {
        "schema_version": 1,
        "theme": {"source": "project"},
        "background": {"kind": "theme"},
        "chrome": {
            "title": "hidden",
            "canvas_padding": 12,
            "surface_gap": 12,
        },
        "pages": [],
        "active_page": 0,
        "id": "marbles-live",
        "project_id": "marbles",
        "revision": revision,
        "layout": {
            "kind": "canvas",
            "placements": [
                {
                    "surface_id": "marbles-summary",
                    "bounds": {"x": 0.01, "y": 0.01, "width": 0.48, "height": 0.31},
                    "layer": 0,
                },
                {
                    "surface_id": "marbles-gantt",
                    "bounds": {"x": 0.50, "y": 0.01, "width": 0.49, "height": 0.60},
                    "layer": 0,
                },
                {
                    "surface_id": "marbles-ready",
                    "bounds": {"x": 0.01, "y": 0.33, "width": 0.48, "height": 0.66},
                    "layer": 0,
                },
                {
                    "surface_id": "marbles-flow",
                    "bounds": {"x": 0.50, "y": 0.62, "width": 0.49, "height": 0.37},
                    "layer": 0,
                },
            ],
        },
        "bindings": [],
        "focused_surface_id": "marbles-summary",
        "surfaces": [
            {
                "id": "marbles-summary",
                "title": "Marbles fleet",
                "content": {
                    "kind": "widget",
                    "spec": {
                        "schema_version": 2,
                        "id": f"marbles-summary{suffix}",
                        "kind": "metric_board",
                        "title": "Marbles fleet",
                        "style": "bullet",
                        "metrics": [
                            {
                                "label": "Ready",
                                "value": int(totals.get("ready") or 0),
                                "max": total,
                                "state": "good",
                            },
                            {
                                "label": "In progress",
                                "value": int(totals.get("in_progress") or 0),
                                "max": total,
                            },
                            {
                                "label": "Review",
                                "value": int(totals.get("review") or 0),
                                "max": total,
                                "state": "warning",
                            },
                            {
                                "label": "Blocked",
                                "value": int(totals.get("blocked") or 0),
                                "max": total,
                                "state": "critical",
                            },
                            {
                                "label": "Stale",
                                "value": int(totals.get("stale") or 0),
                                "max": total,
                                "state": "warning",
                            },
                            {
                                "label": "Done",
                                "value": round(percent_done, 1),
                                "max": 100,
                                "target": 100,
                                "state": "neutral",
                            },
                        ],
                        "data": data,
                    },
                },
                "inputs": [],
            },
            {
                "id": "marbles-ready",
                "title": "Ready work",
                "content": {
                    "kind": "widget",
                    "spec": {
                        "schema_version": 2,
                        "id": f"marbles-ready{suffix}",
                        "kind": "table",
                        "title": "Ready work",
                        "columns": ["Status", "ID", "Project", "Title", "Claim", "Blockers", "Min"],
                        "rows": [card_row(card, "open") for card in open_cards[:READY_ROWS]],
                        "data": data,
                    },
                },
                "inputs": [],
            },
            {
                "id": "marbles-flow",
                "title": "In flight",
                "content": {
                    "kind": "widget",
                    "spec": {
                        "schema_version": 2,
                        "id": f"marbles-flow{suffix}",
                        "kind": "table",
                        "title": "In flight",
                        "columns": ["Status", "ID", "Project", "Title", "Claim", "Blockers", "Min"],
                        "rows": [card_row(card, "in_progress") for card in in_flight[:FLOW_ROWS]],
                        "data": data,
                    },
                },
                "inputs": [],
            },
            {
                "id": "marbles-gantt",
                "title": "Gantt · critical path",
                "content": {
                    "kind": "widget",
                    "spec": {
                        "schema_version": 2,
                        "id": f"marbles-gantt{suffix}",
                        "kind": "table",
                        "title": "Gantt · critical path",
                        "columns": ["Start", "End", "Path", "Status", "ID", "Title", "Min"],
                        "rows": critical_rows[:GANTT_ROWS],
                        "data": data,
                    },
                },
                "inputs": [],
            },
        ],
    }


def canvas_revision(canvasctl: str) -> int | None:
    try:
        result = subprocess.run(
            [canvasctl],
            input="surfaces\n",
            text=True,
            capture_output=True,
            timeout=8,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    match = re.search(r"document rev (\d+)", result.stdout)
    return int(match.group(1)) if match else None


def next_revision(canvasctl: str, state: Path) -> int:
    revisions = [0]
    try:
        revisions.append(int(json.loads(state.read_text(encoding="utf-8")).get("revision", 0)))
    except (OSError, ValueError, TypeError, json.JSONDecodeError):
        pass
    current = canvas_revision(canvasctl)
    if current is not None:
        revisions.append(current)
    return max(revisions) + 1


def write_document(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".canvas-doc-", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(document, handle, separators=(",", ":"))
            handle.write("\n")
        os.replace(temporary, path)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def apply_document(canvasctl: str, path: Path) -> None:
    try:
        result = subprocess.run(
            [canvasctl],
            input=f"doc {path}\n",
            text=True,
            capture_output=True,
            timeout=20,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(f"canvasctl: {error}") from error
    output = (result.stdout + result.stderr).strip()
    if result.returncode != 0 or "ERR" in output or "OK" not in output:
        raise RuntimeError(f"canvasctl apply failed: {output[:800]}")
    log(output.splitlines()[-1])


def refresh(args: argparse.Namespace) -> None:
    dashboard = fetch_dashboard(args)
    revision = next_revision(args.canvasctl, args.state)
    document = build_document(dashboard, revision)
    write_document(args.state, document)
    totals = dashboard.get("totals", {})
    log(
        "fetched "
        f"total={totals.get('total', 0)} ready={totals.get('ready', 0)} "
        f"in_progress={totals.get('in_progress', 0)} "
        f"critical={len(dashboard.get('gantt', {}).get('critical_path', []))} "
        f"revision={revision}"
    )
    if args.dry_run:
        log(f"dry-run doc={args.state}")
        return
    apply_document(args.canvasctl, args.state)


def main() -> int:
    args = parse_args()
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    if args.interval <= 0:
        print("interval must be positive", file=sys.stderr)
        return 2
    while not STOP:
        started = time.monotonic()
        try:
            refresh(args)
        except Exception as error:  # Keep the last good workspace; retry.
            log(f"error: {error}")
            if args.once:
                return 1
        if args.once or STOP:
            break
        elapsed = time.monotonic() - started
        wait = max(1.0, args.interval - elapsed)
        deadline = time.monotonic() + wait
        while not STOP and time.monotonic() < deadline:
            time.sleep(min(0.5, deadline - time.monotonic()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
