// Browser/V8 host for the no-import Marbles view module.
// The same ABI is consumed by the native wasmtime host.

export async function loadView(wasmBytes) {
  const { instance } = await WebAssembly.instantiate(wasmBytes, {});
  const exports = instance.exports;
  for (const name of ["memory", "alloc", "render_block"]) {
    if (!(name in exports)) {
      throw new Error(`marbles-view missing export: ${name}`);
    }
  }
  return exports;
}

export function renderDashboard(exports, dashboard) {
  const input = new TextEncoder().encode(JSON.stringify(dashboard));
  const inputPtr = exports.alloc(input.length);
  new Uint8Array(exports.memory.buffer, inputPtr, input.length).set(input);

  const resultPtr = exports.render_block(inputPtr, input.length);
  const view = new DataView(exports.memory.buffer);
  const length = view.getUint32(resultPtr, true);
  if (length > 16 * 1024 * 1024) {
    throw new Error(`marbles-view result too large: ${length}`);
  }
  const output = new Uint8Array(exports.memory.buffer, resultPtr + 4, length);
  const value = JSON.parse(new TextDecoder().decode(output));
  if (value.error) {
    throw new Error(`marbles-view rejected dashboard: ${value.error}`);
  }
  return value;
}
