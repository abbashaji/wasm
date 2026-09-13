// Run this FIRST, before writing/using harness.mjs.
//
// It answers the one open design question from the handoff empirically:
// does this .wasm need wasm-bindgen-generated JS glue to instantiate at all
// (because getrandom's "wasm_js" backend pulled in wasm-bindgen-shaped
// imports), or can a plain `WebAssembly.instantiate` load it with no
// special imports?
//
// Usage (PowerShell):
//   node inspect-imports.mjs path\to\anthroforge_core.wasm

import { readFile } from "node:fs/promises";

const path = process.argv[2];
if (!path) {
  console.error("usage: node inspect-imports.mjs <path-to-.wasm>");
  process.exit(1);
}

const bytes = await readFile(path);
const mod = await WebAssembly.compile(bytes);

const imports = WebAssembly.Module.imports(mod);
const exports = WebAssembly.Module.exports(mod);

console.log(`--- ${path} ---`);
console.log(`${bytes.length} bytes\n`);

console.log(`IMPORTS (${imports.length}):`);
for (const imp of imports) {
  console.log(`  ${imp.module}.${imp.name}  [${imp.kind}]`);
}

console.log(`\nEXPORTS (${exports.length}):`);
for (const exp of exports) {
  console.log(`  ${exp.name}  [${exp.kind}]`);
}

console.log("\n--- how to read this ---");
console.log(
  "If IMPORTS is empty, or only contains ordinary things you recognize,\n" +
  "a raw WebAssembly.instantiate() harness (see harness.mjs) should work\n" +
  "with an empty or near-empty imports object.\n" +
  "\n" +
  "If IMPORTS contains names like `__wbindgen_*`, `wbg`, or anything that\n" +
  "looks like it expects wasm-bindgen's generated JS runtime to satisfy it\n" +
  "(this can happen because of getrandom's \"wasm_js\" backend, which is\n" +
  "implemented using wasm-bindgen internally, independent of whether this\n" +
  "crate's own exports use #[wasm_bindgen]), paste the full list back.\n" +
  "That changes the recommendation below — either hand-implement exactly\n" +
  "those specific imports (usually just a getRandomValues-shaped call),\n" +
  "or switch getrandom to its \"custom\" backend (a plain Rust callback,\n" +
  "no wasm-bindgen involved) instead of \"wasm_js\". Don't guess here —\n" +
  "this list is the actual, checkable answer.\n" +
  "\n" +
  "Confirm EXPORTS includes: memory, wasm_alloc, wasm_dealloc,\n" +
  "init_part_registry, generate_character, free_mesh_buffer,\n" +
  "anthroforge_last_error. If any are missing, the build didn't pick up\n" +
  "the patched lib.rs."
);
