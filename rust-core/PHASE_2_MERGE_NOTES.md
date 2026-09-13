# Phase 2 Merge Notes — Part A + Part B onto `rust-core`

## What was merged

- **Part A** (`src/lib.rs`, `src/gltf_loader.rs`): replaces the Phase‑1
  single-part `init_part_registry_from_bytes` stub with
  `init_part_registry_from_pack`, which parses the frozen "AnthroForge
  Part Pack" (`.afpp`) byte format (magic/version/index table/data
  region) and populates the global part registry from it in one call.
  Adds `gltf_loader::load_gltf_bytes` so glTF/GLB parts can be parsed
  from an in-memory byte slice (a Part Pack entry) rather than only from
  a filesystem path.
- **Part B** (`src/bin/pack_builder.rs`, `Cargo.toml`): a native,
  standalone `pack_builder` binary that scans a real asset directory
  (numeric-id-prefixed `.obj`/`.glb` files plus `master_skeleton.json`)
  and writes a `.afpp` pack file in the exact format Part A's parser
  expects. `Cargo.toml` gained one `[[bin]]` table to build it.

Applying both parts onto a fresh copy of the base project produced **no
conflicting edits** — Part A touches only `lib.rs`/`gltf_loader.rs`, Part
B touches only the new `pack_builder.rs` and one `[[bin]]` addition to
`Cargo.toml`, exactly matching each part's stated file scope. Confirmed
by diffing the merged tree against the base tree: only those four
changes are present, nothing else.

## Step 1 re-verification — what was actually checked, and the result

I do not have separate hand-off write-up documents from Part A or Part B
in this task's materials (only their delivered diffs/files and the merge
task spec itself) — see "Ambiguities/gaps" below. So verification here
means reading the actual delivered code directly against the task spec's
requirements, not cross-checking a claim document. Result: **no
mismatch found** between what the frozen spec required and what the
delivered code does, specifically:

- `init_part_registry_from_bytes` and any `_impl` for it: confirmed
  fully removed (only referenced in one doc comment, as the thing being
  replaced). No orphaned old function left alongside the new one.
- `init_part_registry_from_pack_impl` (src/lib.rs): confirmed it checks
  the `b"AFPP"` magic, checks `version == 1`, computes every offset with
  `checked_add`/`checked_mul` and rejects any that would put a region
  (skeleton, index table, or a part's `data_offset..data_offset+data_len`)
  past the end of the buffer, and rejects a duplicate `part_id` within
  one pack via `HashMap::insert(...).is_some()`.
- `gltf_loader::load_gltf_bytes`: confirmed it calls
  `gltf::Gltf::from_slice(bytes)` directly (not `Gltf::open` or any other
  file-I/O path), then shares the same mesh/skin extraction code
  (`build_loaded_mesh`) as the file-based loader.
- `pack_builder.rs`: confirmed `.gltf` files are matched explicitly and
  skipped with an `eprintln!` warning (`continue`, not an error and not
  silent acceptance); only `.obj` and `.glb` are accepted as part
  sources. Confirmed a duplicate part id across two files (via the
  numeric filename prefix) returns a hard `Err(...)` from `run()`, using
  a `HashMap<u32, PathBuf>` to name both colliding files, before any
  output is written.
- `Cargo.toml` diff: confirmed the only change from the base file is the
  4-line `[[bin]] name = "pack_builder" path = "src/bin/pack_builder.rs"`
  table appended at the end; nothing else in the file differs.

Both parts' own in-file structural self-tests (`pack_builder.rs`'s
`#[cfg(test)]` module, `lib.rs`'s pack-parsing tests) were also run as
part of the final `cargo test --release` below and passed, but those are
each part's own isolated single-format fixtures — Step 3/4 below is the
real multi-part, mixed-format check this phase specifically calls for.

## Step 3 — asset directory used

**No real production asset directory was available in this environment**
(searched the filesystem; none of the delivered materials includes one).
Per the task's fallback instruction, I built a fixture directory at
`/home/claude/work/fixture_assets/`, deliberately larger/more mixed than
either part's own self-test fixture (which used exactly one part):

| file | type | id | vertices | indices |
|---|---|---|---|---|
| `1001_head.obj` | OBJ | 1001 | 3 | 3 |
| `1002_legs.obj` | OBJ | 1002 | 5 | 9 |
| `2001_arms.glb` | glTF/GLB | 2001 | 3 | 3 |
| `2002_torso.glb` | glTF/GLB | 2002 | 4 | 6 |
| `master_skeleton.json` | — | — | bones: `root`, `joint_arms`, `joint_torso` | |

Four distinct part ids, two formats, genuinely different geometry per
part (not copies of one shape) — a real multi-part, mixed-format input,
not a repeat of either part's single-part self-check.

## Step 4 — real end-to-end pipeline run (native)

```
$ cargo build --release --bin pack_builder
   Finished release [optimized] target(s)

$ ./target/release/pack_builder /home/claude/work/fixture_assets /home/claude/work/real_test.afpp
wrote 4 part(s), 2997 bytes, to '/home/claude/work/real_test.afpp'
```

Correct part count (4) reported, `real_test.afpp` produced.

Native build + a real integration test (`tests/real_pack_e2e.rs`, added
for this verification — not either part's own unit test) then loaded
that real pack and called `generate_character` with `head_id = 1001`
(the OBJ-sourced head) and `torso_id = 2002` (the glTF-sourced torso) —
deliberately one of each source format, as the task asked:

```
$ cargo test --release --test real_pack_e2e -- --nocapture
running 1 test
test real_multi_part_pack_loads_and_generates_character ...
[anthroforge] initialized part registry with 4 part(s) (from pack)
generate_character output: vertices_count=7, indices_count=9
ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

`generate_character` returned non-null output with `vertices_count = 7`,
`indices_count = 9`. This is not just "didn't crash" — it's the exact
expected sum of two **distinct** parts' own counts (head: 3 verts / 3
indices; torso: 4 verts / 6 indices → 7 / 9 combined, matching
`mesh_merge::merge_parts`'s pure-concatenation behavior). If the pipeline
had instead loaded the same part twice, or silently dropped one, this
count would not have come out to exactly 7/9 — so this confirms two real,
different parts were actually merged, not one part duplicated.

## Step 4 (continued) — wasm32 build/behavior check

**Pending user verification.** This sandbox has no `wasm32-unknown-unknown`
target installed and no path to add one (no rustup access; the apt-
installed toolchain is native-only). Per the task, this step is handed
to you to run on a machine with the wasm32 target:

```
cargo build --target wasm32-unknown-unknown --release
node <your wasm harness script>
```

`harness.mjs` already exists in the base project (the Phase 1 pattern
referenced by the task) — reuse/adapt it to instantiate the wasm32
build and call `init_part_registry_from_pack`/`generate_character`
against `real_test.afpp`'s bytes (included in this delivery) the same
way the native test above did, and paste the real output back. I have
not written or guessed at a wasm32 result anywhere in this document.

## Step 5 — full test suite (native, release), final run

```
$ cargo test --release
running 65 tests
... (all pass)
test result: ok. 65 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

     Running unittests src/bin/pack_builder.rs
running 3 tests
... (all pass)
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

     Running tests/real_pack_e2e.rs
running 1 test
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

   Doc-tests anthroforge_core
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

**69 tests total, all passing** (65 lib unit tests + 3 `pack_builder`
unit tests + 1 real multi-part e2e integration test added for this
merge). No failures, no ignored tests.

## Ambiguities / gaps found once real assets were involved

- **No hand-off write-up documents for Part A or Part B were part of
  this task's materials** — only their delivered diffs/full files and
  the base project. The merge task spec asks to check a part's write-up
  claims against its actual code; without those write-ups, Step 1 above
  is a direct code-vs-frozen-spec check instead, which is the stronger
  of the two anyway, but it means I have no external "all tests passed"
  claim from either part to explicitly contradict or confirm — I can
  only report that the delivered code itself matches the spec, which it
  does.
- **No real production asset directory was available** in this sandbox
  (the one the existing native `init_part_registry` directory-scan path
  reads in production). I built the fixture directory described in Step
  3 instead. This is adequate to prove the pipeline's *logic* (real
  multi-part, mixed-format, non-trivial merge), but it does not exercise
  whatever irregularities a real, larger production asset set might
  contain (e.g. many more parts, larger meshes, unusual bone naming).
  Re-running Step 4 against the real asset directory once available
  would be worthwhile before calling this pipeline production-verified.
- **Toolchain constraints in this sandbox required additional dependency
  pinning beyond what Cargo.toml's existing comments already document**
  for `criterion`'s dev-dependency tree: `rayon`/`rayon-core` (needed by
  the main dependency tree, not just dev-dependencies) also required
  pinning to older versions (`rayon = 1.10.0`, `rayon-core = 1.12.1`) to
  build under this sandbox's rustc 1.75.0, in addition to `clap`/
  `clap_lex`/`clap_builder`/`half`/`anstyle` (pinned to `4.5.4`/`0.7.7`/
  `4.5.2`/`2.4.1`/`1.0.6` respectively). These pins are reflected in the
  delivered `Cargo.lock` and are, like the existing criterion pins,
  sandbox-toolchain workarounds — not a statement about what a real CI
  runner with a current stable rustc should use. The original delivered
  `Cargo.lock` in the base zip was also lockfile-version-4, which this
  sandbox's cargo 1.75.0 cannot read at all (`-Znext-lockfile-bump`
  error) — it had to be regenerated from scratch, which is where the new
  pins were applied.

## Addendum reconciliation (post-merge follow-up)

The original merge task's materials did not include the separate
`TASK-ADDENDUM-phase1-gaps` task's output (the `texture_atlas.rs` wasm32
sequential fallback and the `lib.rs` panic-behavior doc-comment
correction) — that output existed but was never provided as an input to
this merge. As delivered, this merge had silently regressed both of
those fixes (texture_atlas.rs was back to an unconditional
`std::thread::scope`, and the doc comment was back to its unqualified
"two-tier defense" claim).

Reconciled directly:
- `src/texture_atlas.rs` replaced wholesale with the addendum's version —
  safe because neither Part A nor Part B ever touched this file, so there
  is no risk of losing merge-specific work.
- `src/lib.rs`'s doc-comment caveat re-applied as a targeted insertion
  (not a wholesale file replace, since Part A's
  `init_part_registry_from_pack` additions to this same file must be
  preserved). Confirmed both now coexist: `init_part_registry_from_pack`
  is intact, and the wasm32 panic caveat is present.

This was not re-verified with a fresh `cargo test --release` run in this
pass — do that before treating this reconciled tree as fully re-verified,
since `texture_atlas.rs` changed after the last real test run recorded
above.
