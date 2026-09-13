#!/usr/bin/env node
/**
 * packages/web/bench/benchmark.mjs
 *
 * Timing/memory/determinism benchmark for `generate()` per
 * TASK-PHASE3-PART-C, §Part 1.
 *
 * STATUS: run for real on 2026-09-13 against the real, merged
 * @anthroforge/web SDK, the real anthroforge_core.wasm, and the real
 * real_test.afpp pack. 200/200 calls succeeded, 0 errors. See
 * PHASE_3_MERGE_NOTES.md for the full real numbers and how this was run
 * (a small Node file://→http fetch shim was added below — the same
 * technique src/index.test.mjs already used — since this Node version's
 * fetch doesn't support file: URLs).
 */

// "@anthroforge/web" is not published and there's no npm workspace set
// up in packages/web/ to make that bare specifier resolve locally, so
// (per the merge — see PHASE_3_MERGE_NOTES.md) this imports the real
// built output directly instead of the placeholder bare specifier.
import { init, generate, getLastError, freeCharacter } from "../dist/index.js";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

// --- Node file:// fetch shim --------------------------------------------
//
// init() uses fetch() for both the bundled wasm module and the Part Pack
// (the one code path that also works unmodified in a browser). This
// Node version's fetch (undici) does not implement the `file:` scheme at
// all ("not implemented... yet..."), and when this script is run the
// normal way (`node bench/benchmark.mjs`), `dist/index.js`'s
// `import.meta.url` — and therefore the wasm URL it derives from it — is
// unavoidably a `file://` URL. src/index.test.mjs hit this identical
// problem and solved it by serving files over a real local HTTP server
// and shimming fetch to rewrite `file://` to `http://` before handing
// off to the real fetch. Reused here verbatim so this script is actually
// runnable with a plain `node bench/benchmark.mjs`, not just in theory.
const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

const fileServer = http.createServer((req, res) => {
  const filePath = path.join(repoRoot, decodeURIComponent(req.url ?? "/"));
  import("node:fs").then(({ readFile }) =>
    readFile(filePath, (err, data) => {
      if (err) {
        res.writeHead(404);
        res.end();
        return;
      }
      res.writeHead(200);
      res.end(data);
    }),
  );
});
const fileServerReady = new Promise((resolve) => fileServer.listen(0, "127.0.0.1", resolve));
const realFetch = globalThis.fetch;
globalThis.fetch = async (input, init) => {
  const url = typeof input === "string" ? new URL(input) : new URL(input.url ?? input);
  if (url.protocol === "file:") {
    await fileServerReady;
    const { port } = fileServer.address();
    const relative = path.relative(repoRoot, url.pathname);
    return realFetch(`http://127.0.0.1:${port}/${relative.split(path.sep).join("/")}`, init);
  }
  return realFetch(input, init);
};

// --- Configuration -----------------------------------------------------

// Path to the real .afpp test pack. Adjust to wherever it actually lives
// relative to this script in the real repo.
const PART_PACK_URL = new URL("../../../real_test.afpp", import.meta.url).href;

const LICENSE_KEY = process.env.ANTHROFORGE_LICENSE_KEY ?? "";

// Real part ids present in real_test.afpp: 1001/1002 (OBJ-text parts),
// 2001/2002 (skinned glTF-binary parts). Part C's original draft could
// not tell which pair the registry treats as "head" vs "torso" from the
// pack alone and guessed HEAD_ID=2001/TORSO_ID=1001. That guess was
// backwards: Part A's index.test.mjs empirically confirmed the real
// mapping is headId=1001, torsoId=2001 (setting out-of-range ids 0-3
// against the live wasm module produced "no part loaded for head_id <N>"
// with N echoing exactly what was set for head_id specifically at 1001,
// and generation succeeds with this pairing but not the reverse).
// Corrected here during the Phase 3 merge — see PHASE_3_MERGE_NOTES.md.
const HEAD_ID = 1001;
const TORSO_ID = 2001;

const TOTAL_CALLS = 200;
const WARMUP_CALLS = 10;

// --- Helpers -------------------------------------------------------------

function makeDNA(callIndex) {
  return {
    seed: BigInt(callIndex + 1), // 200 distinct seeds, 1..200
    heightModifier: 1.0,
    weightModifier: 1.0,
    headId: HEAD_ID,
    torsoId: TORSO_ID,
    clothingIds: [],
  };
}

function percentile(sortedAsc, p) {
  const idx = Math.min(
    sortedAsc.length - 1,
    Math.max(0, Math.ceil((p / 100) * sortedAsc.length) - 1)
  );
  return sortedAsc[idx];
}

function mb(bytes) {
  return (bytes / (1024 * 1024)).toFixed(2);
}

// GeneratedCharacter's real field names, confirmed against the merged
// src/index.ts: positions (Float32Array, 3 per vertex) and indices
// (Uint32Array). The vertexCount fallback below is kept only in case a
// future SDK version adds a precomputed count field.
function readMeshCounts(result) {
  const vertexCount =
    result.vertexCount ??
    (Array.isArray(result.positions) || result.positions?.length !== undefined
      ? result.positions.length / 3
      : undefined);
  const indexCount = result.indices?.length;
  return { vertexCount, indexCount };
}

// --- Main ------------------------------------------------------------

async function main() {
  await init({ partPackUrl: PART_PACK_URL, licenseKey: LICENSE_KEY });

  const heapBefore = process.memoryUsage().heapUsed;
  const rssBefore = process.memoryUsage().rss;

  let firstCallMs = null;
  const steadyStateTimings = []; // calls 11-200 only
  let referenceVertexCount = null;
  let referenceIndexCount = null;
  let meshMismatch = false;
  let errorCount = 0;

  for (let i = 0; i < TOTAL_CALLS; i++) {
    const dna = makeDNA(i);

    const t0 = performance.now();
    const result = generate(dna);
    const t1 = performance.now();
    const elapsedMs = t1 - t0;

    if (result === null) {
      errorCount++;
      console.error(`  call ${i + 1}: generate() returned null: ${getLastError()}`);
      continue;
    }

    if (i === 0) {
      firstCallMs = elapsedMs;
    } else if (i >= WARMUP_CALLS) {
      // calls 11..200 (i is 0-indexed, so i >= 10 means call number >= 11)
      steadyStateTimings.push(elapsedMs);
    }
    // i = 1..9 (calls 2-10): warm-up, intentionally discarded from stats

    const { vertexCount, indexCount } = readMeshCounts(result);
    if (referenceVertexCount === null) {
      referenceVertexCount = vertexCount;
      referenceIndexCount = indexCount;
    } else if (vertexCount !== referenceVertexCount || indexCount !== referenceIndexCount) {
      meshMismatch = true;
    }

    freeCharacter(result);
  }

  if (typeof global.gc === "function") {
    global.gc();
  }

  const heapAfter = process.memoryUsage().heapUsed;
  const rssAfter = process.memoryUsage().rss;

  steadyStateTimings.sort((a, b) => a - b);

  console.log("=== AnthroForge generate() benchmark ===");
  console.log(`Total calls attempted: ${TOTAL_CALLS}`);
  console.log(`Errors: ${errorCount}`);
  console.log("");

  console.log(`First call time: ${firstCallMs !== null ? firstCallMs.toFixed(3) + " ms" : "N/A (first call errored)"}`);
  console.log("");

  console.log(`Steady-state timing, calls 11-${TOTAL_CALLS} (n=${steadyStateTimings.length}):`);
  if (steadyStateTimings.length > 0) {
    console.log(`  min: ${steadyStateTimings[0].toFixed(3)} ms`);
    console.log(`  p50: ${percentile(steadyStateTimings, 50).toFixed(3)} ms`);
    console.log(`  p95: ${percentile(steadyStateTimings, 95).toFixed(3)} ms`);
    console.log(`  p99: ${percentile(steadyStateTimings, 99).toFixed(3)} ms`);
    console.log(`  max: ${steadyStateTimings[steadyStateTimings.length - 1].toFixed(3)} ms`);
  } else {
    console.log("  no successful steady-state calls");
  }
  console.log("");

  console.log("JS-side memory (process.memoryUsage()):");
  console.log(`  heapUsed before: ${mb(heapBefore)} MB`);
  console.log(`  heapUsed after:  ${mb(heapAfter)} MB`);
  console.log(`  heapUsed delta:  ${mb(heapAfter - heapBefore)} MB`);
  console.log(`  rss before:      ${mb(rssBefore)} MB`);
  console.log(`  rss after:       ${mb(rssAfter)} MB`);
  console.log(`  rss delta:       ${mb(rssAfter - rssBefore)} MB`);
  console.log(
    typeof global.gc === "function"
      ? "  (heapUsed/rss 'after' measured following an explicit global.gc() call)"
      : "  (run with `node --expose-gc bench/benchmark.mjs` for a cleaner reading — no GC was forced here)"
  );
  console.log("");

  console.log("Mesh determinism check (fixed headId/torsoId across all calls):");
  console.log(`  vertex count: ${referenceVertexCount}`);
  console.log(`  index count:  ${referenceIndexCount}`);
  console.log(`  identical across all successful calls: ${meshMismatch ? "NO — MISMATCH DETECTED" : "yes"}`);
}

main()
  .catch((err) => {
    console.error("Benchmark failed to run:", err);
    process.exit(1);
  })
  .finally(() => {
    fileServer.close();
  });

/*
 * HANDOFF NOTE — HISTORICAL, kept for context
 * ------------------------------------------------------------------
 * Everything below this line was written before the real
 * anthroforge_core.wasm and real_test.afpp were available, when this
 * script genuinely could not be run. It has since been run for real —
 * see the STATUS comment at the top of this file and
 * PHASE_3_MERGE_NOTES.md for the actual results. Kept as-is below for
 * the historical record of what was and wasn't knowable at each stage.
 *
 * Original Part C note follows:
 *
 * This script has NOT been run, because the actual @anthroforge/web
 * package — the JS/TS wrapper that implements init(), generate(),
 * getLastError(), freeCharacter(), and the CharacterDNA <-> wasm
 * marshalling — was not included in this task's inputs. Only three
 * files were provided: this task's own spec, the compiled
 * anthroforge_core.wasm, and real_test.afpp. There is no
 * packages/web/src/index.ts, index.d.ts, or dist/ build anywhere in
 * this environment, and `@anthroforge/web` is not published on the
 * public npm registry (checked directly — 404).
 *
 * What I *did* verify directly, for real:
 *  - anthroforge_core.wasm is a real, valid wasm module. Its actual
 *    exports are: __getrandom_v03_custom, anthroforge_prewarm_clothing,
 *    free_mesh_buffer, generate_character, init_part_registry,
 *    init_part_registry_from_pack, wasm_alloc, wasm_dealloc,
 *    anthroforge_last_error, free_atlas_buffer, generate_runtime_atlas,
 *    build_cloth_anchors_for_part, fit_clothing_to_character,
 *    free_cloth_anchor_buffer. It imports one host function,
 *    `anthroforge_host.fill_random`. Note this is a richer surface than
 *    the task spec's CharacterDNA/GeneratedCharacter shape suggests
 *    (clothing fitting and runtime atlas generation aren't represented
 *    in that TS interface at all) — consistent with the spec's own
 *    warning that the interface may be stale.
 *  - real_test.afpp is a real, well-formed container: magic "AFPP",
 *    version 1, a small JSON bone-name table (root/pelvis/spine_01/
 *    head), and a table of 4 parts. Two parts (ids 1001, 1002) are
 *    stored as plain OBJ-text triangles; two (ids 2001, 2002) are
 *    stored as skinned glTF-binary blobs. I could not determine from
 *    the pack alone which pair the real part registry classifies as
 *    "head" vs "torso" — that mapping is internal to
 *    init_part_registry_from_pack's Rust implementation, which wasn't
 *    provided and (per the task) I have no toolchain to build anyway.
 *
 * What I could NOT do, and did not fake:
 *  - Actually call init()/generate() through the real public API and
 *    get real timing/memory/mesh numbers, because that API doesn't
 *    exist in this sandbox to call.
 *  - Hand-roll a substitute wrapper myself by calling the raw wasm
 *    exports (generate_character etc.) directly — I know their arities
 *    (all confirmed by instantiating the real module) but not the
 *    struct/memory layout generate_character expects for its input
 *    pointer or produces for its output pointer. Guessing that layout
 *    would risk memory corruption or a crash, and any numbers it
 *    produced wouldn't be measuring the actual shipped SDK anyway —
 *    and writing that wrapper is explicitly out of this task's scope
 *    (packages/web/src/ is owned by a separate task).
 *
 * To actually run this and get real numbers, I need: the real
 * packages/web/src (or a built dist/) for @anthroforge/web, and a
 * valid ANTHROFORGE_LICENSE_KEY for init(). Once those exist, this
 * script should run as-is or with minor field-name fixes (see the
 * `readMeshCounts` TODO above) and the HEAD_ID/TORSO_ID pair should be
 * double-checked against the real registry.
 *
 * ------------------------------------------------------------------
 * ACTUAL RESULTS (real run, 2026-09-13 — see PHASE_3_MERGE_NOTES.md for
 * the full report):
 *   200/200 calls succeeded, 0 errors.
 *   First call: 0.681 ms.
 *   Steady-state (calls 11-200, n=190): min 0.010 ms / p50 0.011 ms /
 *     p95 0.020 ms / p99 0.045 ms / max 0.045 ms.
 *   Mesh determinism: 6 vertices / 6 indices, identical on every call.
 *   heapUsed delta -0.82 MB, rss delta +2.54 MB across 200 calls
 *     (measured after an explicit global.gc(), --expose-gc).
 *   No license key was needed — confirmed unused by the real init()
 *   implementation (see src/index.ts's doc comment on `licenseKey`).
 *   The HEAD_ID/TORSO_ID pair used (1001/2001, corrected from Part C's
 *   original 2001/1001 guess) generated successfully every time.
 */
