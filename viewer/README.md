# Marbles viewer adapters

## Canvas auto-refresh

`canvas_sync.py` projects the read-only `views.dashboard` API into the
`marbles-live` Canvas workspace. It is an adapter, not a second source of
truth: it never writes tracker state and never invents rows when the API is
unavailable.

```sh
CANVASCTL=/path/to/canvasctl python3 viewer/canvas_sync.py --once
CANVASCTL=/path/to/canvasctl python3 viewer/canvas_sync.py --interval 30
```

Defaults:

- API: `http://127.0.0.1:7878` (`--url` / `MARBLES_URL`)
- token: `~/.marbles/tokens/human-avery` (`--token-file` / `MARBLES_TOKEN_FILE`)
- Canvas CLI: `canvasctl` on `PATH` (`--canvasctl` / `CANVASCTL`)
- rendered document: `~/.cache/marbles/canvas-doc.json` (`--state`)

The token is read afresh on every cycle and is never written to the Canvas
document or logs. Redirects are refused so the bearer cannot follow a request
to another host. A fetch or apply failure leaves the last good workspace on
screen and retries on the next interval.

Canvas retains a live widget when its spec id and family are unchanged so
stream-bound values are not discarded. A snapshot refresh is a new value, so
the adapter bumps the widget spec id with the document revision while keeping
the surface id and layout stable. That is what forces the new data to decode.

Run it under a supervisor (tmux, launchd, systemd) for unattended refresh. The
script exits non-zero on a failed `--once` refresh; in loop mode it retries.
