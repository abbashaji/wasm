# AnthroForge/Web — Phase 2 verification complete

Real multi-part Part Pack loading, built and tested end-to-end on the
target machine (Windows, `wasm32-unknown-unknown` + native), by the user.
Every claim below traces to pasted command output.

## Result: Phase 2 milestone reached

- `cargo build --target wasm32-unknown-unknown --release` succeeds.
- `cargo test --release`: 65 passed, 0 failed (lib unit tests) + 3 passed,
  0 failed (`pack_builder` unit tests: structural round-trip, duplicate
  part-id rejection, `.gltf`-skipped-with-warning) + 1 passed, 0 failed
  (`tests/real_pack_e2e.rs` — see below).
- `real_pack_e2e.rs`: `pack_builder` run against a checked-in, portable
  fixture directory (`tests/fixtures/real_pack_e2e/` — 2 `.obj` parts, 2
  `.glb` parts, 4 distinct part ids, plus `master_skeleton.json`) via
  `CARGO_BIN_EXE_pack_builder`, no absolute/host-specific paths. Produced
  a real 4-part, 3002-byte `.afpp` pack; `init_part_registry_from_pack`
  loaded it and `generate_character` produced real, non-null,
  distinct-part mesh output.
- `inspect-imports.mjs` against the real compiled `.wasm`: import list is
  exactly `anthroforge_host.fill_random` — confirmed directly, not
  inferred. No `wasm-bindgen`/`__wbindgen_*` imports of any kind.
- `test-panic-unwind.mjs` against the real compiled `.wasm`:
  `generate_runtime_atlas` returned a valid non-null pointer,
  `anthroforge_last_error()` was `null`, `free_atlas_buffer` completed
  cleanly. The `wasm32` sequential fallback added to `texture_atlas.rs`
  means this call no longer spawns a thread at all, so the panic this
  script was built to trigger no longer occurs on this call path.

## What this does and does not confirm

**Confirmed:** the specific panic source found during Phase 1
(`generate_runtime_atlas_impl`'s un-gated `std::thread::scope` call) is
fixed. Multi-part, mixed OBJ+glTF Part Pack loading works end-to-end on
a real wasm32 build.

**Not re-confirmed here, still standing from Phase 1:** the general
finding that `panic = "unwind"` does not actually unwind on
`wasm32-unknown-unknown` (any *other* internal panic, from a different
cause, would still be expected to trap the module rather than being
recoverable via `catch_unwind`). Nothing in this phase's testing
contradicts that; nothing in this phase's testing re-exercises it either,
since no panic was triggered this run.

## Known deviation from the original task specs, resolved

The merge context's own `tests/real_pack_e2e.rs` used a hardcoded
sandbox-only path (`/home/claude/work/real_test.afpp`), which cannot
exist on the user's machine. The user replaced it with a self-contained
version that builds its own pack via `CARGO_BIN_EXE_pack_builder` against
a fixture directory checked into the repo (`tests/fixtures/real_pack_e2e/`),
with an optional `REAL_AFPP_PATH` env-var override. This is a strict
improvement — portable across machines/OSes, no manual pre-step required
— and has been folded into this project state. No other files were
changed from the merge output.

## Open for the next phase

Per the AnthroForge/Web product spec (v2), §10 Phase 3: `wasm-bindgen` +
`wasm-opt` packaging, real `.wasm` size measurement, real in-browser
benchmark numbers, and publishing `@anthroforge/web` to npm.
