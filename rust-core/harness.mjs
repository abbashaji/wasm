// Raw WebAssembly.instantiate harness — no wasm-bindgen involved.
// Exercises the actual C ABI the native Unreal plugin already depends on.
//
// This drives the new Phase-1-only export, init_part_registry_from_bytes:
// one hardcoded .obj part + master_skeleton.json, both passed as bytes,
// bypassing the directory-scan path that std::fs can't do on wasm32.
//
// Usage (PowerShell), from rust-core\:
//   node harness.mjs `
//     .\target\wasm32-unknown-unknown\release\anthroforge_core.wasm `
//     ..\sample-assets\demo-assets\93011_torso_average.obj `
//     ..\sample-assets\demo-assets\master_skeleton.json

import { readFile } from "node:fs/promises";
import { randomFillSync } from "node:crypto";

const [, , wasmPath, objPath, skeletonPath] = process.argv;
if (!wasmPath || !objPath || !skeletonPath) {
  console.error("usage: node harness.mjs <wasm> <obj-file> <master_skeleton.json>");
  process.exit(1);
}

const bytes = await readFile(wasmPath);

// `memory` is assigned right after instantiation, below — it can't exist
// yet while this `imports` object is being built (the module that will
// export it hasn't been instantiated yet), but that's fine: `fill_random`
// is only actually invoked later, when a call into the module (e.g.
// init_part_registry_from_bytes, via tobj -> ahash -> getrandom) needs
// randomness, which happens well after `memory` is set below.
let memory;

// Real fix for the previous LinkError (was: __wbindgen_placeholder__
// stubs guessed at wasm-bindgen's internal ABI, which is not a small
// fixed surface — the next missing import was only ever one call away).
// Cargo.toml/lib.rs now switch getrandom's wasm32 backend to "custom",
// which needs exactly one plain, explicit import: this one. No
// wasm-bindgen involved anywhere in this build anymore.
const imports = {
  anthroforge_host: {
    fill_random: (ptr, len) => {
      randomFillSync(new Uint8Array(memory.buffer, ptr, len));
    },
  },
};

let instance;
try {
  ({ instance } = await WebAssembly.instantiate(bytes, imports));
} catch (err) {
  console.error("Instantiation failed. This is itself informative:");
  console.error(err.message);
  console.error(
    "\nIf this is a LinkError naming a missing import, run\n" +
    "inspect-imports.mjs first and adjust the `imports` object above.\n" +
    "Do not proceed past this point on a guess."
  );
  process.exit(1);
}

const exports = instance.exports;
memory = exports.memory;
if (!memory) {
  console.error("No exported `memory` — cannot read/write wasm linear memory. Stopping.");
  process.exit(1);
}

for (const name of [
  "wasm_alloc",
  "wasm_dealloc",
  "init_part_registry_from_bytes",
  "generate_character",
  "free_mesh_buffer",
  "anthroforge_last_error",
]) {
  if (typeof exports[name] !== "function") {
    console.error(`Expected export "${name}" not found. Did the build pick up the patched lib.rs?`);
    process.exit(1);
  }
}

// ---- helpers -----------------------------------------------------------

function writeBytes(bytes) {
  const ptr = exports.wasm_alloc(bytes.length);
  new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
  return { ptr, len: bytes.length };
}

function readCString(ptr) {
  if (ptr === 0) return null;
  const view = new Uint8Array(memory.buffer, ptr);
  let end = 0;
  while (view[end] !== 0) end++;
  return new TextDecoder().decode(view.slice(0, end));
}

function lastError() {
  return readCString(exports.anthroforge_last_error());
}

// wasm32 layout of CharacterDNA (pointers are 4 bytes on this target —
// see the handoff's note on why the size assertions are gated out here):
//   seed                        u64  offset 0   (8 bytes)
//   height_modifier             f32  offset 8   (4 bytes)
//   weight_modifier             f32  offset 12  (4 bytes)
//   head_id                     u32  offset 16  (4 bytes)
//   torso_id                    u32  offset 20  (4 bytes)
//   equipped_clothing_ids_ptr   u32  offset 24  (4 bytes, pointer)
//   equipped_clothing_count     u32  offset 28  (4 bytes)
// total 32 bytes
const DNA_SIZE = 32;

function writeCharacterDNA({ seed, height, weight, headId, torsoId }) {
  const ptr = exports.wasm_alloc(DNA_SIZE);
  const view = new DataView(memory.buffer, ptr, DNA_SIZE);
  view.setBigUint64(0, BigInt(seed), true);
  view.setFloat32(8, height, true);
  view.setFloat32(12, weight, true);
  view.setUint32(16, headId, true);
  view.setUint32(20, torsoId, true);
  view.setUint32(24, 0, true); // no equipped clothing for this feasibility test
  view.setUint32(28, 0, true);
  return ptr;
}

// ---- step 1: init_part_registry_from_bytes ------------------------------
// NOTE: this registers ONE part id (93011) as BOTH head_id and torso_id
// below. That's not realistic content — the point is exercising the byte
// -passing + parse + skeleton-resolve + mesh-merge path end to end, not
// producing a sensible-looking character. A real torso merged with itself
// as a "head" is exactly the kind of output that's fine to look odd.

const PART_ID = 93011;
const objBytes = new Uint8Array(await readFile(objPath));
const skeletonBytes = new Uint8Array(await readFile(skeletonPath));

console.log(`Loaded ${objPath} (${objBytes.length} bytes), ${skeletonPath} (${skeletonBytes.length} bytes)`);

const { ptr: objPtr, len: objLen } = writeBytes(objBytes);
const { ptr: skelPtr, len: skelLen } = writeBytes(skeletonBytes);

console.log(`\nCalling init_part_registry_from_bytes(part_id=${PART_ID}, ...)...`);
const initOk = exports.init_part_registry_from_bytes(PART_ID, objPtr, objLen, skelPtr, skelLen);
console.log(`  -> returned: ${initOk}`);
console.log(`  -> anthroforge_last_error(): ${lastError()}`);
exports.wasm_dealloc(objPtr, objLen);
exports.wasm_dealloc(skelPtr, skelLen);

// ---- step 2: generate_character -----------------------------------------

console.log("\nCalling generate_character...");
const dnaPtr = writeCharacterDNA({
  seed: 1n,
  // 1.0, not 0: dna_scale_from_character_dna maps these to
  // [weight_modifier, height_modifier, weight_modifier] as a mesh scale,
  // and mutate_skin_vertices requires every scale component > 0.0 (0
  // would collapse the mesh to a point, not leave it "unmodified" — 1.0
  // is the baseline/no-change value).
  height: 1.0,
  weight: 1.0,
  headId: PART_ID,
  torsoId: PART_ID,
});
const meshPtr = exports.generate_character(dnaPtr);
console.log(`  -> returned pointer: ${meshPtr}`);
console.log(`  -> anthroforge_last_error(): ${lastError()}`);
exports.wasm_dealloc(dnaPtr, DNA_SIZE);

if (meshPtr !== 0) {
  const view = new DataView(memory.buffer, meshPtr, 16);
  const verticesPtr = view.getUint32(0, true);
  const indicesPtr = view.getUint32(4, true);
  const verticesCount = view.getUint32(8, true);
  const indicesCount = view.getUint32(12, true);
  console.log(`  -> vertices_count=${verticesCount} indices_count=${indicesCount}`);
  console.log(
    "\nThis is the real Phase 1 milestone: a real generate_character call,\n" +
    "in a real JS host, against real parsed geometry, with real output.\n" +
    "Paste this whole output back — including the counts above."
  );
  exports.free_mesh_buffer(meshPtr);
} else {
  console.log(
    "\nnull mesh pointer. Paste this whole output back — the exact\n" +
    "anthroforge_last_error() strings above are the real signal, not an\n" +
    "assumption about what 'should' happen."
  );
}
