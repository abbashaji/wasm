// Tests the one thing the Phase 1 handoff explicitly says is still open:
// what actually happens when init_part_registry (the std::fs-based
// directory-scan path — as opposed to init_part_registry_from_bytes,
// which bypasses std::fs entirely) runs on wasm32-unknown-unknown.
//
// Deliberately a SEPARATE script from harness.mjs, not an added step in
// it: instantiating the module fresh here means GLOBAL_REGISTRY starts
// unset, so nothing from a prior init_part_registry_from_bytes call can
// make this result ambiguous (init_part_registry_impl has no "is the
// registry already set?" check before it touches std::fs at all — it
// only discovers that at the very end, via GLOBAL_REGISTRY.set() failing
// — so a shared instance could otherwise mask which failure actually
// fired first).
//
// PREDICTION BEING TESTED, NOT ASSUMED: reading lib.rs, the very first
// fs-touching call in init_part_registry_impl is `asset_dir.is_dir()`.
// Rust's std::path::Path::is_dir() is implemented as
// `fs::metadata(self).map(|m| m.is_dir()).unwrap_or(false)` — it swallows
// any I/O error and returns `false` rather than propagating it. If that
// holds on wasm32-unknown-unknown too, this call should return `false`
// cleanly with a specific last-error message, never reaching
// std::fs::read_dir at all. That's a prediction from reading stdlib
// source, not confirmed for this target — this script is what actually
// checks it. A trap, a hang, or any message other than the predicted one
// is real, new information, not a bug in this test.
//
// Usage (PowerShell):
//   node test-std-fs.mjs .\target\wasm32-unknown-unknown\release\anthroforge_core.wasm

import { readFile } from "node:fs/promises";
import { randomFillSync } from "node:crypto";

const [, , wasmPath] = process.argv;
if (!wasmPath) {
  console.error("usage: node test-std-fs.mjs <wasm>");
  process.exit(1);
}

const bytes = await readFile(wasmPath);

let memory;
const imports = {
  anthroforge_host: {
    // Same custom-getrandom host import as harness.mjs. Not expected to
    // actually be called on this code path (it's only reachable via
    // HashMap::new() inside the directory-entry loop, which the
    // prediction above says is never reached) — present in case that
    // prediction is wrong.
    fill_random: (ptr, len) => {
      randomFillSync(new Uint8Array(memory.buffer, ptr, len));
    },
  },
};

let instance;
try {
  ({ instance } = await WebAssembly.instantiate(bytes, imports));
} catch (err) {
  console.error("Instantiation failed:", err.message);
  process.exit(1);
}

const exports = instance.exports;
memory = exports.memory;
if (!memory) {
  console.error("No exported `memory`. Stopping.");
  process.exit(1);
}

for (const name of ["wasm_alloc", "wasm_dealloc", "init_part_registry", "anthroforge_last_error"]) {
  if (typeof exports[name] !== "function") {
    console.error(`Expected export "${name}" not found.`);
    process.exit(1);
  }
}

function readCString(ptr) {
  if (ptr === 0) return null;
  const view = new Uint8Array(memory.buffer);
  let end = ptr;
  while (view[end] !== 0) end++;
  return new TextDecoder().decode(view.slice(ptr, end));
}

function writeCString(str) {
  const utf8 = new TextEncoder().encode(str);
  const len = utf8.length + 1; // +1 for the NUL terminator
  const ptr = exports.wasm_alloc(len);
  const view = new Uint8Array(memory.buffer, ptr, len);
  view.set(utf8);
  view[utf8.length] = 0;
  return { ptr, len };
}

// This path cannot possibly exist on wasm32-unknown-unknown — there is
// no real filesystem underneath it at all. That's the point: this isn't
// testing "did we type the right path," it's testing what std::fs *does*
// on this target when asked about any path, real or not.
const BOGUS_ASSET_DIR = "/anthroforge/demo-assets";

console.log(`Calling init_part_registry("${BOGUS_ASSET_DIR}")...`);
const { ptr: pathPtr, len: pathLen } = writeCString(BOGUS_ASSET_DIR);
const ok = exports.init_part_registry(pathPtr);
console.log(`  -> returned: ${ok}`);
console.log(`  -> anthroforge_last_error(): ${readCString(exports.anthroforge_last_error())}`);
exports.wasm_dealloc(pathPtr, pathLen);

console.log(
  "\nPaste this whole output back. The exact returned value and error\n" +
  "string above are the real signal:\n" +
  '  - A clean `false` with a message like "does not exist or is not a\n' +
  "    directory\" matches the prediction in this file's header comment\n" +
  "    (Path::is_dir() swallowing the I/O error) — confirms std::fs fails\n" +
  "    informatively here, not silently or by crashing.\n" +
  "  - A trap (e.g. \"unreachable executed\"), a hang, or any other\n" +
  "    message is new information the prediction above got wrong, and\n" +
  "    changes what the real fix needs to be.\n"
);
