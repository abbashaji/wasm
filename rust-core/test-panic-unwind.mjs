// Tests the other item the Phase 1 handoff explicitly flags as open:
// does `panic = "unwind"` + `catch_unwind` actually work on
// wasm32-unknown-unknown, or does an internal panic instead trap the
// whole module as an unrecoverable `RuntimeError`?
//
// HOW THIS TRIGGERS A REAL PANIC (not the crate's own `#[cfg(test)]`
// sentinel — that's compiled out of the release .wasm you built, so it
// can't be used here): `generate_runtime_atlas_impl` (texture_atlas.rs)
// calls `std::thread::scope(|scope| { ... scope.spawn(...) ... })`
// unconditionally for every quadrant, with no wasm32 fallback anywhere
// in that file (checked: zero `wasm32` references in texture_atlas.rs,
// unlike clothing_deformer.rs, which already has one). `Scope::spawn`
// has no `Result` to report failure through — internally it's
// `Builder::new().spawn_scoped(...).expect("failed to spawn thread")`.
// wasm32-unknown-unknown has no real OS thread support, so that spawn is
// expected to fail and that `.expect()` is expected to panic — from
// ordinary, valid input, not a hand-crafted edge case.
//
// PREDICTION BEING TESTED, NOT ASSUMED: `generate_runtime_atlas` wraps
// its body in `panic::catch_unwind`. If wasm32's unwinding support is
// complete enough for that to work, this call should return `null` with
// a specific `anthroforge_last_error()` message containing "internal
// panic" and something about spawning a thread. If wasm32's unwinding
// is incomplete (a real, historical limitation of this target), the
// panic instead traps the whole module — surfacing in Node as an
// uncaught `RuntimeError` thrown across the WebAssembly call boundary,
// which this script catches and reports as a *different* outcome, not
// a script bug.
//
// Usage (PowerShell):
//   node test-panic-unwind.mjs .\target\wasm32-unknown-unknown\release\anthroforge_core.wasm

import { readFile } from "node:fs/promises";
import { randomFillSync } from "node:crypto";

const [, , wasmPath] = process.argv;
if (!wasmPath) {
  console.error("usage: node test-panic-unwind.mjs <wasm>");
  process.exit(1);
}

const bytes = await readFile(wasmPath);

let memory;
const imports = {
  anthroforge_host: {
    // Not expected to be called on this path at all — no HashMap/tobj
    // involved in generate_runtime_atlas. Present for consistency/safety
    // only, same as the other harnesses.
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

for (const name of [
  "wasm_alloc",
  "wasm_dealloc",
  "generate_runtime_atlas",
  "free_atlas_buffer",
  "anthroforge_last_error",
]) {
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

// wasm32 RawImage layout (all fields already 4-byte-aligned, no padding —
// pointers are 4 bytes on this target, same as u32):
//   width        u32  offset 0
//   height       u32  offset 4
//   pixels_ptr   u32  offset 8   (pointer)
//   total_bytes  u32  offset 12
// total 16 bytes
const RAW_IMAGE_SIZE = 16;

// A minimal valid source image: 2x2 RGBA8 (16 bytes of pixel data),
// small enough to fit inside any quadrant of a 4x4 target atlas.
function makeDummyRawImage() {
  const pixelBytes = new Uint8Array(2 * 2 * 4); // all zeros, content is irrelevant
  const pixelsPtr = exports.wasm_alloc(pixelBytes.length);
  new Uint8Array(memory.buffer, pixelsPtr, pixelBytes.length).set(pixelBytes);

  const structPtr = exports.wasm_alloc(RAW_IMAGE_SIZE);
  const view = new DataView(memory.buffer, structPtr, RAW_IMAGE_SIZE);
  view.setUint32(0, 2, true); // width
  view.setUint32(4, 2, true); // height
  view.setUint32(8, pixelsPtr, true); // pixels_ptr
  view.setUint32(12, pixelBytes.length, true); // total_bytes
  return { structPtr, pixelsPtr, pixelsLen: pixelBytes.length };
}

const head = makeDummyRawImage();
const torso = makeDummyRawImage();
const TARGET_ATLAS_SIZE = 4; // even, nonzero; quadrant = 2x2, matches the dummy images exactly

console.log("Calling generate_runtime_atlas(head, torso, null, null, 4)...");
console.log(
  "(head/torso are valid 2x2 dummy images — this is ordinary input, not an\n" +
  " edge case. The panic this is testing comes from thread-spawning, not\n" +
  " from anything wrong with these images.)\n"
);

let outputPtr = 0;
try {
  outputPtr = exports.generate_runtime_atlas(head.structPtr, torso.structPtr, 0, 0, TARGET_ATLAS_SIZE);
  console.log(`  -> returned pointer: ${outputPtr}`);
  console.log(`  -> anthroforge_last_error(): ${readCString(exports.anthroforge_last_error())}`);
} catch (err) {
  console.log("  -> WebAssembly threw across the call boundary (did NOT return cleanly):");
  console.log(`     ${err.constructor.name}: ${err.message}`);
}

if (outputPtr !== 0) {
  try {
    exports.free_atlas_buffer(outputPtr);
    console.log("  -> free_atlas_buffer completed without error");
  } catch (err) {
    console.log(`  -> free_atlas_buffer itself threw: ${err.constructor.name}: ${err.message}`);
  }
}

exports.wasm_dealloc(head.pixelsPtr, head.pixelsLen);
exports.wasm_dealloc(head.structPtr, RAW_IMAGE_SIZE);
exports.wasm_dealloc(torso.pixelsPtr, torso.pixelsLen);
exports.wasm_dealloc(torso.structPtr, RAW_IMAGE_SIZE);

console.log(
  "\nPaste this whole output back. What it means:\n" +
  '  - A clean `0` (null) return with an anthroforge_last_error() message\n' +
  '    like "internal panic: failed to spawn thread" means catch_unwind\n' +
  "    DOES work on wasm32 here — the panic-safety design holds.\n" +
  "  - A caught JS exception (RuntimeError or similar) printed under\n" +
  '    "WebAssembly threw across the call boundary" means catch_unwind\n' +
  "    does NOT recover this panic on wasm32 — it traps the whole module\n" +
  "    instead, which is real, new information the spec (§9 item 4)\n" +
  "    explicitly said not to assume either way.\n" +
  "  - Either result also confirms or denies, in passing, whether\n" +
  "    generate_runtime_atlas's un-gated std::thread::scope call is a\n" +
  "    real problem on this target at all — separate from the\n" +
  "    catch_unwind question itself.\n"
);
