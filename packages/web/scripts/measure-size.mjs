import { readFileSync, existsSync, statSync } from "node:fs";
import { resolve } from "node:path";

const OPTIMIZED_WASM = resolve("dist/anthroforge_core.wasm");
const BUNDLED_JS = resolve("dist/index.js");

// The raw, un-optimized .wasm is whatever the Rust build produced before
// optimize-wasm.mjs ran. It isn't copied anywhere permanent by this
// package's build step, so this only works if it's still sitting where
// the build script last read it from. Override with ANTHROFORGE_WASM_SRC
// if your raw artifact lives somewhere else.
const RAW_WASM =
  process.env.ANTHROFORGE_WASM_SRC ??
  resolve("../core/target/wasm32-unknown-unknown/release/anthroforge_core.wasm");

function bytesToKB(bytes) {
  return (bytes / 1024).toFixed(2);
}

function requireFile(path, label) {
  if (!existsSync(path)) {
    console.error(`measure-size: missing ${label} at ${path}`);
    console.error(`Run "npm run build" first.`);
    process.exit(1);
  }
  return statSync(path).size;
}

const optimizedSize = requireFile(OPTIMIZED_WASM, "optimized .wasm (dist/anthroforge_core.wasm)");
const jsSize = requireFile(BUNDLED_JS, "bundled JS (dist/index.js)");

let rawSize = null;
if (existsSync(RAW_WASM)) {
  rawSize = statSync(RAW_WASM).size;
}

console.log("@anthroforge/web — size report");
console.log("================================");
if (rawSize !== null) {
  console.log(`Raw .wasm:        ${rawSize.toLocaleString()} bytes (${bytesToKB(rawSize)} KB)`);
  console.log(
    `Optimized .wasm:  ${optimizedSize.toLocaleString()} bytes (${bytesToKB(optimizedSize)} KB)`
  );
  console.log(
    `Reduction:        ${(100 * (1 - optimizedSize / rawSize)).toFixed(1)}%`
  );
} else {
  console.log(
    `Raw .wasm:        not found at ${RAW_WASM} (set ANTHROFORGE_WASM_SRC to point at it)`
  );
  console.log(`Optimized .wasm:  ${optimizedSize.toLocaleString()} bytes (${bytesToKB(optimizedSize)} KB)`);
}
console.log(`Bundled JS:       ${jsSize.toLocaleString()} bytes (${bytesToKB(jsSize)} KB)`);
console.log("--------------------------------");
console.log(
  `Combined package size (wasm + js, what a consumer's bundler actually fetches): ` +
    `${(optimizedSize + jsSize).toLocaleString()} bytes (${bytesToKB(optimizedSize + jsSize)} KB)`
);
