//! Phase 2 merge verification: end-to-end native test against a REAL
//! multi-part, mixed-format Part Pack (built by the real `pack_builder`
//! binary from a real fixture asset directory — not either part's own
//! tiny single-format self-check fixture).
//!
//! This is a real integration test (crate compiled as `rlib`), not a
//! throwaway `main.rs`. It builds the pack itself for every run — by
//! invoking the real, already-compiled `pack_builder` binary (located via
//! Cargo's `CARGO_BIN_EXE_pack_builder`, not a guessed path) against the
//! fixture asset directory checked into this repo at
//! `tests/fixtures/real_pack_e2e/` (2 `.obj` parts and 2 `.glb` parts, 4
//! distinct part ids total) — then drives the real FFI entry points
//! against the freshly-built pack. No absolute host path and no
//! pre-existing `.afpp` file are required; this is self-contained and
//! portable across machines/OSes.
//!
//! An explicit `REAL_AFPP_PATH` env var still overrides the pack path
//! entirely (skipping the build step below), for anyone who wants to
//! point this at a hand-built or production pack instead.

use anthroforge_core::{generate_character, init_part_registry_from_pack, CharacterDNA};
use std::path::PathBuf;
use std::process::Command;

/// Builds `real_test.afpp` fresh (via the real `pack_builder` binary
/// against the checked-in fixture asset dir) into a per-test-run tempdir,
/// and returns its bytes. Panics with a descriptive message on any
/// failure, since every step here is expected to succeed in a correctly
/// checked-out repo.
fn build_real_pack_bytes() -> Vec<u8> {
    // Fixture assets ship in the repo itself, resolved relative to this
    // crate's manifest dir rather than any absolute/host-specific path,
    // so this works the same on any machine/OS/checkout location.
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_pack_e2e");
    assert!(
        fixture_dir.is_dir(),
        "expected fixture asset dir at '{}' (checked into the repo) -- did the checkout lose it?",
        fixture_dir.display()
    );

    // Cargo's own tempdir for this test binary's run -- writable,
    // per-test, and cleaned up by cargo; falls back to the OS tempdir if
    // for some reason it isn't set (e.g. a very old cargo).
    let out_dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    std::fs::create_dir_all(&out_dir)
        .unwrap_or_else(|e| panic!("failed to create pack output dir '{}': {e}", out_dir.display()));
    let out_path = out_dir.join("real_pack_e2e_generated.afpp");

    // `CARGO_BIN_EXE_pack_builder` is set by cargo for integration tests
    // to the real, already-built path of the `pack_builder` binary target
    // in this same package -- no guessed `target/release/...` path, and
    // no reliance on the binary having been built by a separate manual
    // step beforehand.
    let pack_builder_bin = env!("CARGO_BIN_EXE_pack_builder");
    let status = Command::new(pack_builder_bin)
        .arg(&fixture_dir)
        .arg(&out_path)
        .status()
        .unwrap_or_else(|e| panic!("failed to run pack_builder at '{pack_builder_bin}': {e}"));
    assert!(
        status.success(),
        "pack_builder exited with failure status building the real test pack from '{}'",
        fixture_dir.display()
    );

    std::fs::read(&out_path)
        .unwrap_or_else(|e| panic!("failed to read freshly-built real pack at '{}': {e}", out_path.display()))
}

#[test]
fn real_multi_part_pack_loads_and_generates_character() {
    let pack_bytes = match std::env::var("REAL_AFPP_PATH") {
        Ok(pack_path) => std::fs::read(&pack_path)
            .unwrap_or_else(|e| panic!("failed to read real pack at '{pack_path}' (from REAL_AFPP_PATH): {e}")),
        Err(_) => build_real_pack_bytes(),
    };

    let ok = init_part_registry_from_pack(pack_bytes.as_ptr(), pack_bytes.len());
    assert!(
        ok,
        "init_part_registry_from_pack must succeed loading the real 4-part pack"
    );

    // head_id = 1001 (OBJ-sourced, 3 verts / 3 indices)
    // torso_id = 2002 (glTF-sourced, 4 verts / 6 indices)
    // These are two DISTINCT real parts of different formats and shapes.
    let dna = CharacterDNA {
        seed: 42,
        height_modifier: 1.0,
        weight_modifier: 1.0,
        head_id: 1001,
        torso_id: 2002,
        equipped_clothing_ids_ptr: std::ptr::null(),
        equipped_clothing_count: 0,
    };

    let output_ptr = generate_character(&dna as *const CharacterDNA);
    assert!(
        !output_ptr.is_null(),
        "generate_character must return non-null output for two real, loaded, distinct parts"
    );

    let output = unsafe { &*output_ptr };
    println!(
        "generate_character output: vertices_count={}, indices_count={}",
        output.vertices_count, output.indices_count
    );

    // Sanity: non-zero.
    assert!(output.vertices_count > 0, "vertices_count must be nonzero");
    assert!(output.indices_count > 0, "indices_count must be nonzero");

    // Sanity: merged counts must reflect BOTH distinct parts being
    // concatenated (mesh_merge::merge_parts just concatenates), not one
    // part duplicated or the other silently dropped.
    // head (OBJ, 3 verts/3 indices) + torso (glTF, 4 verts/6 indices)
    // = 7 vertices, 9 indices, exactly.
    assert_eq!(
        output.vertices_count, 7,
        "expected exactly 7 vertices (3 head + 4 torso) -- got {}; a wrong count here would mean \
         one part was duplicated or dropped rather than two distinct parts merged",
        output.vertices_count
    );
    assert_eq!(
        output.indices_count, 9,
        "expected exactly 9 indices (3 head + 6 torso) -- got {}",
        output.indices_count
    );
}
