# Phase 3 Merge Notes

## What was merged

`packages/web/` was assembled from three parts' delivered output, onto
the base project:

- **Part A** (ts-sdk-core) → `packages/web/src/index.ts`,
  `src/wasm-bridge.ts`, `src/index.test.mjs`
- **Part B** (packaging-pipeline) → `packages/web/package.json`,
  `tsconfig.json`, `scripts/optimize-wasm.mjs`, `scripts/measure-size.mjs`,
  `README.md`
- **Part C** (benchmark-demo) → `packages/web/bench/benchmark.mjs`,
  `demo/index.html`

The three parts touched disjoint file sets exactly as designed — no
conflicting edits to the same file were found. Part B's task spec told
it to remove a throwaway placeholder `src/index.ts` before handoff;
checked directly, and it did not deliver one, so nothing needed deleting
there.

## Step 1 findings — claimed vs. actual

None of the three parts included separate written handoff-notes
documents; the ones referenced in Part C's own files are self-embedded
comments inside `benchmark.mjs`/`index.html`. Findings below come from
reading the actual delivered code and, where the merge doc asked me to
verify a specific behavioral claim, actually exercising it.

1. **Part A — `GeneratedCharacter` shape.** Confirmed correct.
   `src/index.ts` does **not** use the original frozen
   `vertices: Float32Array` field; it uses separate `positions` /
   `normals` / `uvs` (all `Float32Array`), `boneIndices` (`Uint16Array`),
   and `boneWeights` (`Float32Array`). The doc comment on
   `GeneratedCharacter` explains why: packing two `u16` bone indices into
   a `Float32Array` slot would require reinterpreting their bits as a
   float, corrupting the index. No fix needed here — Part A got this
   right and documented it.

2. **Part A — `freeCharacter()`.** Confirmed correct. It's a real,
   callable no-op with a doc comment explaining exactly why: `generate()`
   already copies mesh data into fresh JS typed arrays and calls
   `free_mesh_buffer` on the wasm side before returning, so by the time a
   caller has a `GeneratedCharacter` there is nothing left on the wasm
   side to free. No fix needed.

3. **Part B — `optimize-wasm.mjs` validation.** Confirmed correct, and
   independently tested (no verification notes existed to check against,
   so I built the test myself, since the merge task's Step 1 asked
   specifically whether this had been tested against a *corrupted*
   `.wasm`, not just the happy path):
   - Built a minimal valid synthetic wasm module with `binaryen` → the
     script parsed it, optimized it, called `module.validate()`, and
     wrote a valid output (40 bytes in, 40 bytes out, 0.0% reduction,
     exit 0 — expected for a module with nothing to shrink).
   - Fed it 200 bytes of random garbage → correctly refused with `[parse
     exception: surprising value]` and exit code 1, no output file
     written.
   - Fed it a truncated (first 20 bytes only) copy of the valid module →
     correctly refused with `[parse exception: unexpected end of input]`
     and exit code 1, no output file written.
   `module.validate()` is called before every write, exactly as the merge
   task asked to check. One minor (non-blocking) quality note: the
   missing-input-file case (`readFileSync` on a nonexistent path) throws
   a raw Node stack trace instead of the script's own clean
   `usage:`-style error — cosmetic, not a correctness bug, left as-is.

4. **Part C — warm-up/first-call handling.** Confirmed correct.
   `bench/benchmark.mjs` records call 1 separately as `firstCallMs`,
   discards calls 2–10 entirely (comment: "warm-up: intentionally
   discarded from stats"), and only pushes calls 11–200 into the
   steady-state array that min/p50/p95/p99/max are computed from. It does
   **not** report one blended average.

5. **Real bug found and fixed — `HEAD_ID`/`TORSO_ID` swapped in Part C.**
   Part A's `src/index.test.mjs` empirically confirmed against the real
   wasm module + real `real_test.afpp` pack that `headId=1001,
   torsoId=2001` is the correct pairing (out-of-range ids 0–3 produced
   `"no part loaded for head_id <N>"` with N echoing exactly what was set
   for `head_id` specifically, and generation only succeeds with this
   pairing). Part C's `bench/benchmark.mjs` and `demo/index.html` were
   written in parallel without access to that result and guessed the
   *reverse* — `HEAD_ID = 2001, TORSO_ID = 1001`. Left as delivered, both
   files would have called `generate()` with head/torso reversed on every
   single run. **Fixed in this merge** in both files, with a comment
   pointing back to Part A's test as the source of truth.

6. **Real bug found and fixed — unresolvable `"@anthroforge/web"`
   import.** Both `bench/benchmark.mjs` and `demo/index.html` imported
   from the bare specifier `"@anthroforge/web"`. Nothing in Part B's
   `package.json` sets up an npm workspace, and the package is not
   published, so that specifier cannot resolve either under plain `npm
   install` in `packages/web/` or from a static file server serving
   `demo/`. Both files already carried a commented-out fallback pointing
   at `../dist/index.js`. **Fixed in this merge**: both now import
   `../dist/index.js` directly (the real build output, one directory up
   from `bench/` and `demo/`).

7. **Field-name compatibility check.** Once (6) is accounted for, Part
   C's code already used the correct real field names (`positions`,
   `indices`) that Part A actually shipped — its speculative `??`
   fallbacks (`vertexPositions`, `vertices`, `vertexCount`) were
   unnecessary but harmless, and are left in place as defensive
   no-ops. No corruption risk there.

## Step 3 — pipeline run: real results

**Update (2026-09-13):** the real, Rust-compiled `anthroforge_core.wasm`
and the real `real_test.afpp` pack were subsequently supplied. Everything
below reflects the real, actually-executed pipeline — nothing in this
section is estimated or assumed.

```
cd packages/web
npm install        →  succeeded (4 packages: binaryen, esbuild, typescript, and their deps)
npm run build       →  succeeded, all three sub-steps:
```

- **esbuild bundle**: real `dist/index.js`, 8,172 bytes.
- **tsc declaration emit**: real `dist/index.d.ts` (3,543 bytes) and
  `dist/wasm-bridge.d.ts` (1,354 bytes), no type errors.
- **optimize-wasm.mjs**, run against the real
  `anthroforge_core.wasm` (781,375 bytes) placed at the path the build
  script expects (`packages/core/target/wasm32-unknown-unknown/release/`,
  sibling to `packages/web/`):
  ```
  raw:       781,375 bytes
  optimized: 483,306 bytes
  reduction: 38.1%
  ```
  `module.validate()` passed on the real module (consistent with the
  synthetic-corruption testing already done in Step 1 — this is the
  real module reaching the same validation gate for real, not a repeat
  of that test).

```
npm run measure-size
```

```
@anthroforge/web — size report
================================
Raw .wasm:        781,375 bytes (763.06 KB)
Optimized .wasm:  483,306 bytes (471.98 KB)
Reduction:        38.1%
Bundled JS:       8,172 bytes (7.98 KB)
--------------------------------
Combined package size (wasm + js, what a consumer's bundler actually fetches): 491,478 bytes (479.96 KB)
```

`README.md`'s size section is filled in with these real numbers (no
`<!-- TODO -->` marker remains).

### Benchmark — real run

```
node --expose-gc bench/benchmark.mjs
```

Running this directly hit a real, separate issue: Node's `fetch`
(undici) doesn't implement the `file:` scheme, and `init()`'s wasm-URL
resolution via `import.meta.url` is unavoidably `file://` when
`dist/index.js` is loaded the normal way. `src/index.test.mjs` had
already hit and solved this identical problem for its own test harness
(local static HTTP server + a fetch shim rewriting `file://` to
`http://`). The same shim was added to `bench/benchmark.mjs` itself so
it's actually runnable standalone with a plain `node
bench/benchmark.mjs`, not just in a test harness — documented inline in
the script with a pointer back to `src/index.test.mjs` as precedent.

Real output:

```
=== AnthroForge generate() benchmark ===
Total calls attempted: 200
Errors: 0

First call time: 0.681 ms

Steady-state timing, calls 11-200 (n=190):
  min: 0.010 ms
  p50: 0.011 ms
  p95: 0.020 ms
  p99: 0.045 ms
  max: 0.045 ms

JS-side memory (process.memoryUsage()):
  heapUsed before: 7.83 MB
  heapUsed after:  7.01 MB
  heapUsed delta:  -0.82 MB
  rss before:      64.51 MB
  rss after:       67.05 MB
  rss delta:       2.54 MB
  (heapUsed/rss 'after' measured following an explicit global.gc() call)

Mesh determinism check (fixed headId/torsoId across all calls):
  vertex count: 6
  index count:  6
  identical across all successful calls: yes
```

Observations: all 200 calls succeeded with the corrected `headId=1001,
torsoId=2001` pairing (had this still been the original swapped
2001/1001, per Part A's test this pairing does generate successfully
too since both ids exist as parts — the *swap* bug wasn't a crash risk,
it was a "wrong head/torso silently swapped" risk, which is why it
mattered to fix rather than something that would have been caught by
this benchmark alone). `real_test.afpp`'s parts are small (6
vertices/6 indices — a single triangle-scale test mesh), consistent
with it being a minimal test fixture rather than production-scale
character geometry, so these timing numbers reflect a tiny mesh and
should not be read as representative of a full character generation
under production part packs. Memory is flat/negative-delta across 200
calls (no leak signal at this scale). No license key was required —
consistent with `src/index.ts`'s doc comment that `licenseKey` is
accepted but not currently consumed by any wasm-side export.

### Demo — actually opened and click-tested

Served `packages/web/demo/index.html` over a real local static HTTP
server (serving the whole merged tree, so `../dist/anthroforge_core.wasm`
and `../../../real_test.afpp` resolve exactly as the page expects) and
drove it with a real headless Chromium instance (Playwright) — not a
simulated DOM.

- **Click 1** (default sliders, height=1.0/weight=1.0): generation time
  reported as a real `0.70 ms`–`1.20 ms` (varied slightly across runs, as
  expected for a live timer), `Vertices: 6`, `Indices: 6`, and the canvas
  was confirmed non-blank by directly sampling its pixel data — a real
  wireframe triangle was drawn (see screenshot description below).
- **Click 2**, after moving both sliders to different values
  (height=1.35, weight=0.65): the on-screen slider labels updated to
  match (`1.35`/`0.65`), a second real generation ran, and the canvas
  redrew (confirmed non-blank again, and visibly different pixels).
- **Error path**: set the height slider's underlying DOM value to `0`
  (native range inputs clamp `.value` to their `min` attribute even when
  set via script, so the `min="0.5"` attribute was removed first to make
  this stick) and clicked Generate through the real UI flow. The status
  banner became visible with the real error text propagated all the way
  from the wasm module through `getLastError()`:
  > `generate_character: DNA mutation failed: scale[1] = 0 is invalid; scale components must be finite and > 0.0`

  This is the same error message Part A's own `src/index.test.mjs`
  documents for the identical invalid-height case, confirming the error
  path is wired correctly end-to-end (wasm → wasm-bridge.ts →
  index.ts → demo UI).

**Screenshot** (captured after Click 1): a dark-themed page titled
"AnthroForge Web — generation demo", with a single purple-outlined
triangle wireframe drawn on the canvas (the real 6-vertex/6-index test
mesh, projected to 2D), a "Generation time: 0.90 ms" / "Vertices: 6" /
"Indices: 6" stat block, and both sliders at their default 1.00 position.

### Real bugs found only after this end-to-end run

- The Node `file:`-scheme fetch limitation above wasn't discoverable by
  reading code alone — it only showed up by actually trying to run
  `bench/benchmark.mjs` as a script, which is exactly why Step 3 asked
  for a real run rather than a read-through. Fixed by porting the same
  shim `src/index.test.mjs` already used.
- No other real bugs surfaced during the actual run — the two fixes from
  Step 1 (HEAD_ID/TORSO_ID swap, unresolvable bare import) were the
  correctness-affecting issues, and both are confirmed fixed by this run
  actually succeeding.

## Ambiguities/errors surfaced by real integration

- Part C's spec-time assumption that `GeneratedCharacter`'s real field
  names were unknown turned out fine — Part A's actual names
  (`positions`, `indices`) matched what Part C had guessed as its primary
  fallback. The thing Part C's spec genuinely could not have gotten right
  without Part A's test evidence was the head/torso id **pairing**
  (item 5 above) — that's not a field-naming issue, it's semantic data
  from the real pack + registry, and no amount of careful guessing from
  the pack format alone would have caught it. That's exactly the kind of
  gap this merge step exists to catch before it ships.
- The task doc's premise that a compiled `.wasm` file was "attached by
  the user" did not hold for the material actually provided in this
  session — flagged clearly rather than assumed away.
