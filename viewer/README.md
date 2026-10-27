# Marbles viewer adapters

## Canvas auto-refresh

`marbles-canvas-sync` projects the read-only `views.dashboard` API into the
`marbles-live` Canvas workspace. It is an adapter, not a second source of truth:
it never writes tracker state and never invents rows when the API is
unavailable.

```sh
cargo build --bin marbles-canvas-sync
CANVASCTL=/path/to/canvasctl target/debug/marbles-canvas-sync --once
CANVASCTL=/path/to/canvasctl target/debug/marbles-canvas-sync --interval 30
```

Defaults:

- API: `http://127.0.0.1:7878` (`--url` / `MARBLES_URL`)
- token: `MARBLES_TOKEN`, else `MARBLES_TOKEN_FILE`, else
  `~/.marbles/tokens/human-avery`
- Canvas CLI: `canvasctl` on `PATH` (`--canvasctl` / `CANVASCTL`)
- rendered document: `~/.cache/marbles/canvas-doc.json` (`--state`)

The token is read afresh on every cycle and is never written to the Canvas
document or logs. A fetch or apply failure leaves the last good workspace on
screen and retries on the next interval.

Canvas retains a live widget when its spec id and family are unchanged so
stream-bound values are not discarded. A snapshot refresh is a new value, so
the adapter bumps the widget spec id with the document revision while keeping
the surface id and layout stable. That is what forces the new data to decode.

Run it under a supervisor (tmux, launchd, systemd) for unattended refresh. The
binary exits non-zero on a failed `--once` refresh; in loop mode it retries.

For remote Canvas endpoints, `unibus-canvas` now has a generic bounded
`canvas.workspace.apply` operation and `canvas-fleet apply DOCUMENT TARGETS`.
Marbles-specific API mapping stays in this repository; Unibus carries the
already-materialized document.
