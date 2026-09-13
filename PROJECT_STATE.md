# AnthroForge/Web — project state snapshot

## What's in this snapshot

- `rust-core/` — Phase 2-complete, plus two direct Phase 4 fixes:
  - `equipped_clothing_ids_ptr`'s stale "not wired up" doc comment
    corrected (it IS wired — `generate_character` fits and merges every
    equipped item into the output mesh).
  - `anthroforge_prewarm_clothing`'s unconditional `std::thread::spawn`
    (would trap on wasm32, no OS threads there) fixed with a synchronous
    `wasm32` fallback, same pattern as the earlier `texture_atlas.rs` fix.
  - `packages/web/src/index.test.mjs` has a new real test proving
    `clothingIds` actually merges into `generate()`'s output — this path
    was wired on both ends but never actually exercised until now.
- `packages/web/` — Phase 3-complete: the `@anthroforge/web` SDK core,
  build pipeline, benchmark, and demo page. All verified against real
  builds.
- `packages/web-three/` — Phase 4's three.js adapter,
  `@anthroforge/web-three`. Verified for real: a `.afpp` pack was
  hand-built from the checked-in fixture assets (came out
  byte-identical in size — 3002 bytes — to the real `pack_builder`
  tool's own output, confirming the format is implemented consistently
  on both ends), linked against the real built `@anthroforge/web`, and
  its test suite was actually executed (not just read): both tests
  pass (`toBufferGeometry()` produces a real, correctly-shaped
  `THREE.BufferGeometry`; `hasAtlas()` correctly reports `false` against
  today's atlas-less `generate()` output).

## What's explicitly NOT done yet — the next step

Skinned rendering IS done (see below) — this section now describes what
comes after it.

## Skinned rendering — DONE, verified for real

`@anthroforge/web` now exposes `getSkeleton()`, wrapping the real
`get_skeleton()`/`free_skeleton_buffer` wasm exports (44-byte `FfiJoint`,
8-byte `SkeletonBuffer` on wasm32). `@anthroforge/web-three` now exposes
`toSkinnedMesh(character, skeleton, material?)`, building a real
`THREE.Bone` hierarchy, calling `updateMatrixWorld()` before constructing
`THREE.Skeleton` (the easy-to-miss step that silently produces identity
inverse binds if skipped), and binding a real `THREE.SkinnedMesh`.

Verified by actually running the tests against the real, current
`anthroforge_core.wasm` (confirmed via `WebAssembly.Module.exports` to
genuinely contain `get_skeleton`/`free_skeleton_buffer` — 17 exports
total): all 4 tests in `packages/web-three/src/index.test.mjs` pass,
including a real non-trivial 3-joint (`root → spine → head`) fixture
built fresh in JS (`src/fixtures/hierarchy-pack.mjs`) whose expected
values were independently hand-verified (the 90°-rotation +
non-uniform-scale joint's quaternion was recomputed by hand and matches),
and a direct check that `boneInverses[1]`/`[2]` are non-identity — the
specific check that would catch a skipped `updateMatrixWorld()` call.

Note: the checked-in `tests/fixtures/real_pack_e2e/master_skeleton.json`
was trimmed (by the Rust skeleton-extraction task) to a single `"root"`
bone, since neither of that fixture's `.glb` files ever contributes
`pelvis`/`spine_01`/`head`. Any `.afpp` built against the old 4-bone
skeleton file will now correctly fail to load (the completeness check
added in that same task catches it) — this is working as intended, not a
regression, and was directly observed during this task's verification.

## What's next
