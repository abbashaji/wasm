import { readFileSync, writeFileSync } from "node:fs";
import binaryen from "binaryen";

const inputPath = process.argv[2];
const outputPath = process.argv[3];
if (!inputPath || !outputPath) {
  console.error("usage: node optimize-wasm.mjs <input.wasm> <output.wasm>");
  process.exit(1);
}

const rawBytes = readFileSync(inputPath);

let module;
try {
  module = binaryen.readBinary(rawBytes);
} catch (err) {
  console.error(
    `optimize-wasm: failed to parse "${inputPath}" as a valid wasm module: ${err.message ?? err}`
  );
  process.exit(1);
}

module.optimize(); // uses binaryen's default -O optimization passes

// Explicit size-focused pass, since the default `optimize()` level is
// tuned for a balance of size/speed, not purely size — this SDK is
// distributed to browsers where download size matters more than a small
// runtime speed difference (per the product spec's size-budget framing).
binaryen.setShrinkLevel(2);
binaryen.setOptimizeLevel(2);
module.optimize();

// Correctness check, not just a size check: don't trust the optimized
// output until binaryen itself confirms the module is still valid.
const isValid = module.validate();
if (!isValid) {
  console.error(
    `optimize-wasm: binaryen produced an invalid module from "${inputPath}". ` +
      "Refusing to write a corrupted .wasm file."
  );
  module.dispose();
  process.exit(1);
}

const optimizedBytes = module.emitBinary();
writeFileSync(outputPath, optimizedBytes);

console.log(`raw:       ${rawBytes.length.toLocaleString()} bytes`);
console.log(`optimized: ${optimizedBytes.length.toLocaleString()} bytes`);
console.log(
  `reduction: ${(100 * (1 - optimizedBytes.length / rawBytes.length)).toFixed(1)}%`
);

module.dispose();
