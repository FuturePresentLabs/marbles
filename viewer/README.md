# Marbles viewer adapters

## Canvas auto-refresh

`marbles-canvas-sync` is the host. The dashboard -> widget projection is **not**
in the host: it lives in `viewer/marbles-view`, compiled to a no-import core
wasm module. The host fetches the authenticated `views.dashboard` response, runs
the module under wasmtime, and applies the result through the owner's local
Canvas socket.

```sh
# Build the projection artifact (once, or after changing the projection).
cargo build --manifest-path viewer/marbles-view/Cargo.toml \
  --target wasm32-unknown-unknown --release

# Build and run the host.
cargo build --bin marbles-canvas-sync
CANVASCTL=/path/to/canvasctl target/debug/marbles-canvas-sync --once
CANVASCTL=/path/to/canvasctl target/debug/marbles-canvas-sync --interval 30
```

Defaults:

- API: `http://127.0.0.1:7878` (`--url` / `MARBLES_URL`)
- token: `MARBLES_TOKEN`, else `MARBLES_TOKEN_FILE`, else
  `~/.marbles/tokens/human-avery`
- Canvas CLI: `canvasctl` on `PATH` (`--canvasctl` / `CANVASCTL`)
- wasm artifact: `viewer/marbles-view/target/wasm32-unknown-unknown/release/marbles_view.wasm`
  (`--wasm` / `MARBLES_VIEW_WASM`)
- rendered document: the platform cache directory, e.g.
  `~/Library/Caches/marbles/canvas-doc.json` on macOS or
  `${XDG_CACHE_HOME:-~/.cache}/marbles/canvas-doc.json` on Linux (`--state`)

The token is read afresh on every cycle and is never written to the Canvas
document, wasm memory, or logs. A fetch, wasm render, or apply failure leaves the
last good workspace on screen and retries on the next interval.

## The wasm ABI

Core wasm, no imports:

```text
memory: exported
alloc(len: i32) -> ptr: i32
render_block(ptr: i32, len: i32) -> ptr: i32
```

Input is `marbles.view.dashboard.v1` JSON plus a host-supplied `view_revision`.
The result is a length-prefixed block (`u32 LE length + UTF-8 JSON`) containing
the Canvas workspace document, or `{"error":"..."}`. The native host uses
wasmtime; the browser/V8 host uses `WebAssembly.instantiate` with the exact same
module and ABI.

```sh
node viewer/marbles-view/web/smoke.mjs
```

That runs the browser host against the same wasm artifact and fixture used by
the native tests.

Canvas retains a live widget when its spec id and family are unchanged so
stream-bound values are not discarded. A snapshot refresh is a new value, so the
wasm projection puts the document revision in the widget spec id while keeping
the surface id and layout stable. That is what forces the new data to decode.

Run the host under a supervisor (tmux, launchd, systemd) for unattended refresh.
The binary exits non-zero on a failed `--once` refresh; in loop mode it retries.

For remote Canvas endpoints, `unibus-canvas` has a generic bounded
`canvas.workspace.apply` operation and `canvas-fleet apply DOCUMENT TARGETS`.
Marbles-specific API mapping stays in this repository; Unibus carries the
already-materialized document.
