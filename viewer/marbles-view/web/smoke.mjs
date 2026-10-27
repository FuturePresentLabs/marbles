// Run the exact browser host against the wasm artifact under Node/V8.
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { loadView, renderDashboard } from "./host.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const wasmPath = process.env.MARBLES_VIEW_WASM
  ?? resolve(here, "../target/wasm32-unknown-unknown/release/marbles_view.wasm");
const fixturePath = resolve(here, "../tests/fixtures/dashboard.json");

const wasm = await readFile(wasmPath);
const dashboard = JSON.parse(await readFile(fixturePath, "utf8"));
const exports = await loadView(wasm);
const document = renderDashboard(exports, { ...dashboard, view_revision: 7 });

if (document.revision !== 7) throw new Error("revision not propagated");
if (document.surfaces?.length !== 4) throw new Error("expected four surfaces");
if (document.surfaces[1].content.spec.rows.length !== 1) {
  throw new Error("ready card was not filtered");
}
if (document.surfaces[1].content.spec.rows[0][1] !== "ready") {
  throw new Error("wrong ready card");
}
console.log(`marbles-view browser host ok · ${wasm.length} bytes · revision ${document.revision}`);
