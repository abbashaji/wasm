//! Anthroforge Runtime Procedural Character Generation Engine — Rust core.
//!
//! Reconciled surface after Phases 1-4 (see PHASE_5 write-up for the full
//! diff-by-diff reconciliation notes and the two real gaps this file fixes
//! or flags: `free_mesh_buffer` was documented everywhere but never
//! implemented, and `clothing_deformer`/most of `texture_atlas` are wired
//! into the crate but not yet reachable from `generate_character`).
//!
//! # Safety contract for callers across the FFI boundary
//! - `init_part_registry` must be called exactly once, at boot time,
//!   before any call to `generate_character`.
//! - `asset_dir` must be a valid, non-null, NUL-terminated UTF-8 C string
//!   for the duration of the call.
//! - `dna` passed to `generate_character` must be a valid, non-null,
//!   correctly-aligned pointer to a fully-initialized `CharacterDNA` for
//!   the duration of the call.
//! - The `MeshOutputBuffer*` returned by `generate_character` (and the
//!   `vertices_ptr`/`indices_ptr` arrays it points at) become owned by the
//!   caller. They must eventually be released via `free_mesh_buffer`.
//!   Do not `free()` them with any allocator other than this library's.
//! - The `RuntimeAtlasOutput*` returned by `generate_runtime_atlas` must
//!   eventually be released via `free_atlas_buffer`.
//!
//! No function in this crate panics by design. Every fallible internal
//! operation returns a `Result` that is converted to a `bool`/null-pointer
//! sentinel at the `extern "C"` boundary.
//!
//! # Panic behavior at the FFI boundary (Test 03 finding)
//! The release profile sets `panic = "unwind"` (changed from `"abort"` —
//! see `Cargo.toml`'s comment and `RESULTS-03.md` for the empirical
//! verification and full reasoning). This is a two-tier defense, and
//! callers should understand both tiers:
//!
//! **This framing assumes a target where `panic = "unwind"` actually
//! unwinds.** On `wasm32-unknown-unknown`, it does not: confirmed
//! empirically (AnthroForge/Web wasm port) by triggering a genuine
//! internal panic through a real compiled `.wasm` in a real JS host and
//! observing the module trap (`RuntimeError: unreachable`) rather than
//! recovering through `catch_unwind`. On that target there is
//! effectively **no surviving tier** — any internal panic anywhere traps
//! the whole module, regardless of which of the two tiers below it
//! originated in. Everything below this note is verified only for the
//! native build; it remains correct there, it just does not carry across
//! targets.
//!
//! - `generate_runtime_atlas`/`free_atlas_buffer` (`texture_atlas.rs`)
//!   wrap their bodies in `std::panic::catch_unwind` and convert a caught
//!   panic into a clean `null`/no-op return. With `panic = "unwind"` this
//!   now actually works, confirmed empirically: an internal panic no
//!   longer crashes the process for these two exports.
//! - `init_part_registry` and `generate_character` do **not** wrap
//!   themselves in `catch_unwind`. If an internal panic ever occurs in
//!   their call graph, Rust's own FFI-unwind guard converts the escaping
//!   unwind into a process abort at the `extern "C"` boundary (this
//!   happens regardless of the crate's `panic` profile setting, because
//!   neither function is declared `extern "C-unwind"`) — confirmed
//!   empirically in `RESULTS-03.md`. In other words, switching the crate
//!   to `panic = "unwind"` did not change these two exports' behavior at
//!   all: they still abort on any internal panic, exactly as before.
//!   They were audited and rely only on this abort-on-escape guarantee,
//!   not on `panic = "abort"` specifically, for correctness — see
//!   `RESULTS-03.md` for the audit (`GLOBAL_REGISTRY` is only written
//!   after the local `parts` map is fully built, so a panic before that
//!   point leaves it untouched; a panic after it cannot occur because
//!   nothing panic-prone runs after the write).
//!
//! C++ callers should treat every export in this crate as fallible-by-
//! crash for internal panics that occur outside the two guarded atlas
//! functions, and treat `generate_runtime_atlas`/`free_atlas_buffer` as
//! panic-safe (null/no-op on internal panic, no crash).

mod body_mutation;
mod clothing_deformer;
mod error;
mod gltf_loader;
mod mesh_merge;
mod obj_loader;
mod skeleton_resolver;
mod texture_atlas;

pub use clothing_deformer::{
    build_cloth_anchors_for_part, fit_clothing_to_character, free_cloth_anchor_buffer,
};
pub use error::anthroforge_last_error;
pub use texture_atlas::{free_atlas_buffer, generate_runtime_atlas};

use error::{clear_last_error, set_last_error};

use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::OnceLock;

// ============================================================================
// FFI struct layouts — exact, byte-for-byte, verified against the C++ mirror.
// ============================================================================

#[derive(Clone, Copy)]
#[repr(C)]
pub struct SkinnedVertex {
    pub position: [f32; 3],     // offset 0,  12 bytes
    pub normal: [f32; 3],       // offset 12, 12 bytes
    pub uv: [f32; 2],           // offset 24, 8 bytes
    pub bone_indices: [u16; 4], // offset 32, 8 bytes
    pub bone_weights: [f32; 4], // offset 40, 16 bytes
}
// Total size MUST be 56 bytes (12+12+8+8+16), zero padding.

#[repr(C)]
pub struct CharacterDNA {
    pub seed: u64,             // offset 0, 8 bytes
    pub height_modifier: f32,  // offset 8, 4 bytes
    pub weight_modifier: f32,  // offset 12, 4 bytes
    pub head_id: u32,          // offset 16, 4 bytes
    pub torso_id: u32,         // offset 20, 4 bytes
    /// Caller-owned array of equipped clothing part ids (same id space as
    /// `head_id`/`torso_id`, resolved against the part registry). May be
    /// null iff `equipped_clothing_count == 0` ("nothing equipped").
    ///
    /// CORRECTED (an earlier version of this comment, from before the
    /// DNA-mutation step existed, said this field was not consulted by
    /// `generate_character` and had to be fitted manually per item via
    /// `build_cloth_anchors_for_part`/`fit_clothing_to_character` — that
    /// went stale once the integration landed and was never updated):
    /// `generate_character` DOES read this field. For each id, it resolves
    /// the part, builds/reuses cached anchors via
    /// `Registry::get_or_build_clothing_anchors`, fits it against the
    /// already-mutated body skin via `fit_clothing_to_skin`, and merges the
    /// result into the single returned mesh alongside the body. A missing
    /// part id, missing anchors, or a fit failure for one item is logged
    /// and that one item is skipped — it does not fail the whole call (see
    /// `generate_character`'s own doc comment). Calling
    /// `build_cloth_anchors_for_part`/`fit_clothing_to_character` directly
    /// is no longer a separate required path for a caller who just wants
    /// clothing on a generated character; it remains available for anyone
    /// who wants to fit an item outside the normal generation flow (e.g. a
    /// preview/try-on UI against an already-generated skin buffer).
    pub equipped_clothing_ids_ptr: *const u32, // offset 24, 8 bytes
    /// Number of ids in `equipped_clothing_ids_ptr`. `0` means "nothing
    /// equipped" (and `equipped_clothing_ids_ptr` may then be null).
    pub equipped_clothing_count: u32, // offset 32, 4 bytes
    // 4 bytes of trailing padding to keep 8-byte pointer alignment.
}
// Total size MUST be 40 bytes (8+4+4+4+4 + 8+4, padded to 8-byte
// alignment because of `equipped_clothing_ids_ptr`). This grew from
// Phase 1-4's 24 bytes; every call site must be rebuilt against the new
// layout (source-compatible field-by-field construction, e.g. C++'s
// `FfiDna.Field = ...` style already used in
// `AnthroforgeCharacterAssembler.cpp`, is unaffected in the fields it
// already sets — only ABI size changes, not the meaning of any existing
// field).

#[repr(C)]
pub struct MeshOutputBuffer {
    pub vertices_ptr: *mut SkinnedVertex, // offset 0, 8 bytes
    pub indices_ptr: *mut u32,            // offset 8, 8 bytes
    pub vertices_count: u32,              // offset 16, 4 bytes
    pub indices_count: u32,               // offset 20, 4 bytes
}
// Field order is mandatory: both pointers BEFORE both counts. Reordering to
// (ptr, count, ptr, count) silently pads the struct to 32 bytes on a 64-bit
// target and will desync from the C++ mirror. Total size MUST be 24 bytes.

const _: () = assert!(std::mem::size_of::<SkinnedVertex>() == 56);

// CharacterDNA and MeshOutputBuffer contain raw pointers
// (equipped_clothing_ids_ptr / vertices_ptr / indices_ptr). These
// assertions exist to catch this struct drifting out of sync with its
// hand-written C++ mirror (AnthroforgeCoreTypes.h) on the native
// (cdylib) build, where pointers are 8 bytes and Rust/C++ must agree
// byte-for-byte on layout across the FFI boundary.
//
// wasm32-unknown-unknown is a 32-bit target: pointers are 4 bytes there,
// so the real sizes are legitimately smaller (32, not 40; 16, not 24).
// There is no C++ mirror on the wasm path at all, and — despite an
// earlier version of this comment claiming otherwise — this project does
// not use wasm-bindgen at all (see `wasm_alloc`/`wasm_dealloc` below and
// `.cargo/config.toml`'s getrandom "custom" backend, chosen specifically
// to avoid pulling wasm-bindgen's ABI in). The JS side marshals
// `CharacterDNA` by hand, writing these exact fields at these exact
// byte offsets into linear memory it obtained from `wasm_alloc` — so the
// native-ABI-parity premise these assertions check does not apply to
// that target, but for the opposite reason the old comment gave: not
// because a JS-object marshaller papers over layout differences, but
// because there is no marshaller at all and the JS wrapper must already
// know the real wasm32 layout precisely (see the AnthroForge/Web SDK's
// packaging docs for those offsets).
#[cfg(not(target_arch = "wasm32"))]
const _: () = assert!(std::mem::size_of::<CharacterDNA>() == 40);
#[cfg(not(target_arch = "wasm32"))]
const _: () = assert!(std::mem::size_of::<MeshOutputBuffer>() == 24);

/// One bone of the fully-assembled global skeleton, as exposed by
/// [`get_skeleton`]. `parent_index` is `-1` for the root (a JS caller
/// builds one `THREE.Bone` per `FfiJoint`, `.add()`s each onto its
/// `parent_index`'s bone, and constructs `new THREE.Skeleton(bones)` with
/// no second argument — `THREE.Skeleton` computes correct inverse bind
/// matrices itself from this bind-pose hierarchy).
#[repr(C)]
pub struct FfiJoint {
    pub parent_index: i32, // -1 for "no parent" (this is the root)
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

/// Caller-owned output of [`get_skeleton`]: a heap array of [`FfiJoint`]
/// plus its length, mirroring `MeshOutputBuffer`/`ClothAnchorBuffer`'s
/// `(ptr, count)` shape. Must be released via [`free_skeleton_buffer`],
/// and by no other allocator.
#[repr(C)]
pub struct SkeletonBuffer {
    pub joints_ptr: *mut FfiJoint,
    pub joint_count: u32,
}

// `FfiJoint` contains no pointers — only `i32`/`f32` fields, all already
// 4-byte-aligned, and the struct's own alignment is therefore 4 — so
// unlike every other `#[repr(C)]` type in this file (CharacterDNA,
// MeshOutputBuffer, and SkeletonBuffer just below, all of which contain a
// pointer and are legitimately smaller on wasm32's 4-byte pointers), its
// layout does NOT depend on target pointer width: 4 (parent_index) + 12
// (translation) + 16 (rotation) + 12 (scale) = 44 bytes with zero padding
// on both native and wasm32. Verified, not assumed — this assertion is
// therefore deliberately left un-gated (checked on every target,
// including wasm32) rather than following the `#[cfg(not(target_arch =
// "wasm32"))]` pattern the pointer-containing structs use.
const _: () = assert!(std::mem::size_of::<FfiJoint>() == 44);

// `SkeletonBuffer` DOES contain a pointer (`joints_ptr`), so — like
// CharacterDNA/MeshOutputBuffer above — its size is only asserted on the
// native (64-bit pointer) target; wasm32's 4-byte pointers give a
// legitimately smaller size there (12, not 16), with no C++ mirror to
// keep in sync against on that path.
#[cfg(not(target_arch = "wasm32"))]
const _: () = assert!(std::mem::size_of::<SkeletonBuffer>() == 16);

// ============================================================================
// In-memory registry, populated once at init time.
// ============================================================================

/// A single loaded, already skeleton-resolved modular part.
struct PartData {
    vertices: Vec<SkinnedVertex>,
    indices: Vec<u32>,
}

// SkinnedVertex/PartData contain only plain data (no interior mutability,
// no raw pointers), so they are trivially Send + Sync. We spell that out
// explicitly rather than relying on auto-trait inference, since
// `GLOBAL_REGISTRY` being `Sync` is load-bearing for this module.
unsafe impl Send for PartData {}
unsafe impl Sync for PartData {}

/// Sample every Nth skin vertex when building a shared per-body KD-tree
/// for clothing-anchor matching. 1 = no decimation (exact, slower to
/// build once per body). Higher = faster per-body tree build/query, at
/// the cost of coarser anchor matching (clipping prevention is
/// unaffected either way — see `clothing_deformer::build_skin_kdtree`'s
/// doc comment).
const SKIN_KDTREE_DECIMATION_STRIDE: usize = 4;

/// Extra outward push, beyond an anchor's own `thickness_clearance`,
/// applied by `clothing_deformer::fit_clothing_to_skin`'s SDF push-out
/// step (see that function's doc comment). A small positive margin so a
/// fitted clothing vertex ends up strictly outside the skin surface
/// rather than resting exactly on the minimum-safe boundary, where
/// floating-point rounding could otherwise let it read as clipping.
const CLOTHING_CLEARANCE_EPSILON: f32 = 0.001;

struct Registry {
    parts: HashMap<u32, PartData>, // existing field, unchanged
    /// The fully-assembled global skeleton (`master_skeleton.json`'s bone
    /// hierarchy plus every bone's real bind-pose local transform,
    /// contributed across every loaded part — see `skeleton_resolver`),
    /// indexed by global bone index. Populated once, at
    /// `init_part_registry*` time, and guaranteed complete (every index
    /// `Some`, in the `skeleton_resolver::GlobalJoint` sense) by the
    /// completeness check both init paths run before constructing this
    /// `Registry` — see `check_global_joints_complete`. Read by
    /// `get_skeleton`.
    global_joints: Vec<skeleton_resolver::GlobalJoint>,
    /// Per-body (`head_id`, `torso_id`) shared skin KD-tree, plus the
    /// full-resolution merged skin buffer it was built from (needed by
    /// `clothing_deformer::build_cloth_anchors_with_tree`). Deliberately a
    /// separate tier from `clothing_anchor_cache` so every clothing item
    /// fitted to the same body reuses one tree entry instead of each
    /// triggering its own tree rebuild.
    skin_tree_cache: std::sync::RwLock<
        HashMap<(u32, u32), std::sync::Arc<(clothing_deformer::SkinKdTree, Vec<SkinnedVertex>)>>,
    >,
    /// Per-(`head_id`, `torso_id`, `clothing_id`) fitted anchors, built and
    /// cached on first request.
    clothing_anchor_cache:
        std::sync::RwLock<HashMap<(u32, u32, u32), std::sync::Arc<Vec<clothing_deformer::ClothAnchor>>>>,
}

/// Every fallible operation in the clothing-anchor cache path returns one
/// of these. Mirrors `clothing_deformer::ClothingDeformerError`'s
/// `Display`/`Error` style.
#[derive(Debug)]
pub enum ClothingAnchorError {
    /// `head_id` did not name a part loaded into the registry.
    UnknownHeadId(u32),
    /// `torso_id` did not name a part loaded into the registry.
    UnknownTorsoId(u32),
    /// `clothing_id` did not name a part loaded into the registry.
    UnknownClothingId(u32),
    /// Merging the head/torso parts into one skin buffer failed.
    MeshMerge(mesh_merge::MeshMergeError),
    /// Building the shared skin tree or the clothing item's anchors
    /// against it failed.
    Deformer(clothing_deformer::ClothingDeformerError),
}

impl fmt::Display for ClothingAnchorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClothingAnchorError::UnknownHeadId(id) => {
                write!(f, "no part loaded for head_id {id}")
            }
            ClothingAnchorError::UnknownTorsoId(id) => {
                write!(f, "no part loaded for torso_id {id}")
            }
            ClothingAnchorError::UnknownClothingId(id) => {
                write!(f, "no part loaded for clothing_id {id}")
            }
            ClothingAnchorError::MeshMerge(e) => write!(f, "failed to merge head/torso parts: {e}"),
            ClothingAnchorError::Deformer(e) => write!(f, "clothing deformer failed: {e}"),
        }
    }
}

impl std::error::Error for ClothingAnchorError {}

impl Registry {
    /// Returns the shared, decimated skin KD-tree for this (head, torso)
    /// pair, building and caching it on first request. Every clothing
    /// item fitted to this same body reuses this one tree instead of
    /// rebuilding it.
    fn get_or_build_skin_tree(
        &self,
        head_id: u32,
        torso_id: u32,
    ) -> Result<std::sync::Arc<(clothing_deformer::SkinKdTree, Vec<SkinnedVertex>)>, ClothingAnchorError>
    {
        // Fast path: an existing entry is reused as-is.
        {
            let cache = self.skin_tree_cache.read().unwrap();
            if let Some(entry) = cache.get(&(head_id, torso_id)) {
                return Ok(std::sync::Arc::clone(entry));
            }
        }

        let head = self
            .parts
            .get(&head_id)
            .ok_or(ClothingAnchorError::UnknownHeadId(head_id))?;
        let torso = self
            .parts
            .get(&torso_id)
            .ok_or(ClothingAnchorError::UnknownTorsoId(torso_id))?;

        let merged = mesh_merge::merge_parts(&[
            (head.vertices.as_slice(), head.indices.as_slice()),
            (torso.vertices.as_slice(), torso.indices.as_slice()),
        ])
        .map_err(ClothingAnchorError::MeshMerge)?;
        let (merged_vertices, _merged_indices) = merged;

        let tree = clothing_deformer::build_skin_kdtree(&merged_vertices, SKIN_KDTREE_DECIMATION_STRIDE)
            .map_err(ClothingAnchorError::Deformer)?;

        let entry = std::sync::Arc::new((tree, merged_vertices));

        // Duplicate-build-under-race note: two threads racing on the same
        // brand-new (head, torso) pair may both build once here; the
        // second write below simply overwrites the first with an
        // equivalent result. Intentional — no per-key locking is added to
        // prevent it, matching the rest of this crate's caching
        // conventions.
        let mut cache = self.skin_tree_cache.write().unwrap();
        cache.insert((head_id, torso_id), std::sync::Arc::clone(&entry));
        Ok(entry)
    }

    /// Returns this (head, torso, clothing) combination's anchors,
    /// building and caching them on first request. Internally reuses the
    /// shared per-body tree from `get_or_build_skin_tree` rather than
    /// rebuilding one per clothing item.
    pub(crate) fn get_or_build_clothing_anchors(
        &self,
        head_id: u32,
        torso_id: u32,
        clothing_id: u32,
    ) -> Result<std::sync::Arc<Vec<clothing_deformer::ClothAnchor>>, ClothingAnchorError> {
        // Fast path: an existing entry is reused as-is.
        {
            let cache = self.clothing_anchor_cache.read().unwrap();
            if let Some(entry) = cache.get(&(head_id, torso_id, clothing_id)) {
                return Ok(std::sync::Arc::clone(entry));
            }
        }

        // Resolve the clothing id up front, before doing any building, so
        // an unresolvable id never triggers (or caches) a partial build.
        let clothing_part = self
            .parts
            .get(&clothing_id)
            .ok_or(ClothingAnchorError::UnknownClothingId(clothing_id))?;

        // Builds the shared per-body tree if this is the first request for
        // this body at all; returns the cached one instantly otherwise.
        let tree_entry = self.get_or_build_skin_tree(head_id, torso_id)?;

        let anchors = clothing_deformer::build_cloth_anchors_with_tree(
            &clothing_part.vertices,
            &tree_entry.1,
            &tree_entry.0,
        )
        .map_err(ClothingAnchorError::Deformer)?;

        let anchors = std::sync::Arc::new(anchors);

        // Same duplicate-build-under-race note as `get_or_build_skin_tree`
        // applies here too: intentional, no per-key locking.
        let mut cache = self.clothing_anchor_cache.write().unwrap();
        cache.insert((head_id, torso_id, clothing_id), std::sync::Arc::clone(&anchors));
        Ok(anchors)
    }
}

// ============================================================================
// Clothing-anchor prewarming — optional, additive latency-hiding entry
// point. Every code path in `Registry::get_or_build_clothing_anchors`
// already works correctly without this ever being called; this exists
// purely so a caller (e.g. a loading-screen hook) can populate the cache
// ahead of time for known (head, torso, clothing) combinations instead of
// paying the first-use cost during a real spawn.
// ============================================================================

/// Synchronous implementation — builds and caches every requested
/// `clothing_id` against `(head_id, torso_id)`, discarding results (the
/// point is populating the cache, not returning anything). Individual
/// failures (a bad clothing id, etc.) are logged to stderr and skipped,
/// same as everywhere else in this crate — one bad id in a prewarm batch
/// must not abort the rest of the batch.
pub(crate) fn prewarm_clothing_anchors_impl(
    registry: &Registry,
    head_id: u32,
    torso_id: u32,
    clothing_ids: &[u32],
) {
    for &clothing_id in clothing_ids {
        if let Err(e) = registry.get_or_build_clothing_anchors(head_id, torso_id, clothing_id) {
            eprintln!(
                "[anthroforge] prewarm_clothing_anchors: skipping clothing_id {clothing_id} for \
                 (head_id {head_id}, torso_id {torso_id}): {e}"
            );
        }
    }
}

/// Populated exactly once by `init_part_registry`. Reads from
/// `generate_character` (potentially from multiple engine worker threads)
/// are safe and lock-free after that single write, since `OnceLock` only
/// ever hands out a shared reference to fully-initialized data.
static GLOBAL_REGISTRY: OnceLock<Registry> = OnceLock::new();

// ============================================================================
// Phase 9a — bench-only surface. Compiled out entirely unless `--features
// bench` is passed (see Cargo.toml's `bench` feature doc comment). Pure
// pass-through to the existing `pub(crate)` build_skin_kdtree — added so
// the decimation-stride sweep benchmark in
// benches/generate_character_bench.rs can drive an explicit stride from
// outside the crate. generate_character's real call path
// (Registry::get_or_build_skin_tree) is completely untouched and still
// always uses the fixed SKIN_KDTREE_DECIMATION_STRIDE constant.
// ============================================================================
#[cfg(feature = "bench")]
pub fn bench_only_build_skin_kdtree(
    skin_vertices: &[SkinnedVertex],
    stride: usize,
) -> Result<(), String> {
    clothing_deformer::build_skin_kdtree(skin_vertices, stride)
        .map(|_tree| ())
        .map_err(|e| e.to_string())
}

// ============================================================================
// extern "C" exports
// ============================================================================

/// Parse every `.gltf`/`.glb` file (full skin data, via `gltf_loader`) and
/// every `.obj` file (no skin data; rigidly bound via `obj_loader`) directly
/// inside `asset_dir` against `asset_dir/master_skeleton.json`, and
/// populate the global, thread-safe part cache.
///
/// Boot-time only: intended to be called exactly once. A second call
/// returns `false` without modifying the already-initialized registry.
///
/// # Safety
/// `asset_dir` must be a valid, non-null, NUL-terminated C string that
/// stays valid for the duration of this call.
///
/// # Returns
/// `true` if the registry was freshly initialized with at least one part.
/// `false` on any failure (bad path, bad UTF-8, missing/malformed
/// `master_skeleton.json`, zero loadable parts, or the registry having
/// already been initialized). Diagnostic detail is written to stderr;
/// this function itself never panics.
#[no_mangle]
pub extern "C" fn init_part_registry(asset_dir: *const c_char) -> bool {
    // Cleared unconditionally at entry so a caller that checks
    // `anthroforge_last_error()` after a *successful* call never sees a
    // stale message left over from some earlier failing call on this
    // thread.
    clear_last_error();

    match init_part_registry_impl(asset_dir) {
        Ok(part_count) => {
            eprintln!("[anthroforge] initialized part registry with {part_count} part(s)");
            true
        }
        Err(message) => {
            eprintln!("[anthroforge] init_part_registry failed: {message}");
            set_last_error(format!("init_part_registry failed: {message}"));
            false
        }
    }
}

/// Production multi-part loading entry point (AnthroForge/Web spec §4.4):
/// parses a single, pre-built "AnthroForge Part Pack" buffer — a
/// `master_skeleton.json` plus N modular parts (each either `.obj` text or
/// glTF/GLB binary), all packed into one flat byte buffer by a trusted,
/// out-of-scope authoring tool — and populates the global, thread-safe
/// part cache from it in one call. This is the production replacement for
/// the earlier Phase-1 single-part `init_part_registry_from_bytes`
/// feasibility stub (removed).
///
/// Like `init_part_registry`, this may only succeed once per process — a
/// second call (to this function, `init_part_registry`, or any other init
/// path) returns `false` without modifying the already-initialized
/// registry.
///
/// Unlike `init_part_registry`'s directory scan, a malformed pack fails
/// the whole call rather than silently loading a partial registry — see
/// `init_part_registry_from_pack_impl`'s doc comment for why.
///
/// # Safety
/// `pack_bytes_ptr` must be valid for reads of `pack_bytes_len` bytes for
/// the duration of this call.
///
/// # Returns
/// `true` if every part in the pack was parsed, skeleton-resolved, and
/// registered successfully. `false` on any failure (bad magic/version,
/// truncated index, an out-of-bounds part offset, an unparseable part, a
/// bone name not present in the skeleton, a duplicate `part_id`, or the
/// registry having already been initialized). Diagnostic detail is
/// available via `anthroforge_last_error()`, exactly as for
/// `init_part_registry`.
#[no_mangle]
pub extern "C" fn init_part_registry_from_pack(
    pack_bytes_ptr: *const u8,
    pack_bytes_len: usize,
) -> bool {
    clear_last_error();

    match init_part_registry_from_pack_impl(pack_bytes_ptr, pack_bytes_len) {
        Ok(part_count) => {
            eprintln!(
                "[anthroforge] initialized part registry with {part_count} part(s) (from pack)"
            );
            true
        }
        Err(message) => {
            eprintln!("[anthroforge] init_part_registry_from_pack failed: {message}");
            set_last_error(format!("init_part_registry_from_pack failed: {message}"));
            false
        }
    }
}

/// Frozen "AnthroForge Part Pack" (`.afpp`) byte format this function
/// parses:
///
/// ```text
/// offset 0   : magic       b"AFPP"           (4 bytes, ASCII, no NUL)
/// offset 4   : version     u32 little-endian = 1
/// offset 8   : part_count  u32 little-endian (N, N >= 1)
/// offset 12  : skel_len    u32 little-endian
/// offset 16  : skeleton    skel_len raw bytes (master_skeleton.json, verbatim)
/// then N fixed-size (13-byte) index entries, back-to-back:
///              part_id     u32 little-endian
///              src_type    u8    (0 = OBJ text, 1 = glTF/GLB binary)
///              data_offset u32 little-endian (absolute, from buffer start)
///              data_len    u32 little-endian
/// then the N parts' raw source bytes, addressed only by data_offset/
/// data_len — never assumed contiguous or in index order.
/// ```
///
/// Every field is read via explicit little-endian byte reads
/// (`u32::from_le_bytes`), never a pointer-cast/`#[repr(C)]` read over the
/// raw buffer, so there is no alignment requirement on any offset.
///
/// Unlike `init_part_registry_impl`'s directory scan (which skips a bad
/// individual file and logs a warning, since a real asset folder can
/// contain stray non-part files), this function fails the whole call on
/// the first bad part. A Part Pack is a single artifact produced by a
/// trusted build tool, so a malformed one indicates a real bug worth
/// surfacing loudly, not a condition to route around.
/// Checks that every bone named in `master.name_to_global_index` has been
/// contributed by some loaded part (i.e. `master.global_joints` is fully
/// `Some`). Deliberately a free function, independent of `GLOBAL_REGISTRY`,
/// so it's unit-testable without the process-wide-singleton flakiness the
/// rest of this module's `#[cfg(test)]` tests have to work around (see
/// those tests' own comments).
///
/// On failure, names every still-missing bone — reusing
/// `name_to_global_index` via a small reverse map built just for this
/// error message (no need to store one permanently; this only ever runs
/// once, at init time).
fn check_global_joints_complete(master: &skeleton_resolver::MasterSkeleton) -> Result<(), String> {
    let missing_indices: Vec<u32> = master
        .global_joints
        .iter()
        .enumerate()
        .filter(|(_, joint)| joint.is_none())
        .map(|(index, _)| index as u32)
        .collect();

    if missing_indices.is_empty() {
        return Ok(());
    }

    let reverse: HashMap<u32, &str> = master
        .name_to_global_index
        .iter()
        .map(|(name, &index)| (index, name.as_str()))
        .collect();

    let mut missing_names: Vec<&str> = missing_indices
        .iter()
        .map(|index| *reverse.get(index).unwrap_or(&"<unnamed bone>"))
        .collect();
    missing_names.sort_unstable();

    Err(format!(
        "master skeleton names {} bone(s) that no loaded part ever contributed hierarchy/bind-pose data for: {}",
        missing_names.len(),
        missing_names.join(", ")
    ))
}

/// Converts a `MasterSkeleton`'s `global_joints: Vec<Option<GlobalJoint>>`
/// into the fully-resolved `Vec<GlobalJoint>` the `Registry` stores, once
/// `check_global_joints_complete` has already confirmed every slot is
/// `Some`. Never panics even if that invariant were somehow violated —
/// this crate's existing convention — returning a (should-be-unreachable)
/// internal-error `Err` instead of unwrapping.
fn finalize_global_joints(
    master: skeleton_resolver::MasterSkeleton,
) -> Result<Vec<skeleton_resolver::GlobalJoint>, String> {
    let mut resolved = Vec::with_capacity(master.global_joints.len());
    for (index, joint) in master.global_joints.into_iter().enumerate() {
        match joint {
            Some(j) => resolved.push(j),
            None => {
                return Err(format!(
                    "internal error: check_global_joints_complete reported completeness but global bone index {index} is still unresolved"
                ))
            }
        }
    }
    Ok(resolved)
}

fn init_part_registry_from_pack_impl(
    pack_bytes_ptr: *const u8,
    pack_bytes_len: usize,
) -> Result<usize, String> {
    if pack_bytes_ptr.is_null() {
        return Err("pack_bytes_ptr was null".to_string());
    }
    if GLOBAL_REGISTRY.get().is_some() {
        return Err("registry already initialized".to_string());
    }

    // SAFETY: caller contract above guarantees this is valid for reads of
    // `pack_bytes_len` bytes for the duration of this call.
    let bytes = unsafe { slice::from_raw_parts(pack_bytes_ptr, pack_bytes_len) };

    const HEADER_LEN: usize = 16;
    const INDEX_ENTRY_LEN: usize = 13;

    if bytes.len() < HEADER_LEN {
        return Err(format!(
            "pack buffer is too short to contain a header: {} byte(s), need at least {HEADER_LEN}",
            bytes.len()
        ));
    }

    let magic = &bytes[0..4];
    if magic != b"AFPP" {
        return Err(format!(
            "bad magic bytes: found {magic:02x?}, expected {:02x?} (b\"AFPP\")",
            b"AFPP"
        ));
    }

    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != 1 {
        return Err(format!("unsupported pack version: found {version}, expected 1"));
    }

    let part_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let skel_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;

    let skeleton_start = HEADER_LEN;
    let skeleton_end = skeleton_start
        .checked_add(skel_len)
        .ok_or_else(|| "skeleton length overflows buffer offset arithmetic".to_string())?;
    if skeleton_end > bytes.len() {
        return Err(format!(
            "skeleton bytes (offset {skeleton_start}..{skeleton_end}) exceed the pack buffer's {} byte(s)",
            bytes.len()
        ));
    }
    let skeleton_bytes = &bytes[skeleton_start..skeleton_end];

    let mut master_skeleton = skeleton_resolver::parse_master_skeleton_bytes(
        skeleton_bytes,
        Path::new("<embedded pack bytes>"),
    )
    .map_err(|e| e.to_string())?;

    let index_start = skeleton_end;
    let index_len = part_count
        .checked_mul(INDEX_ENTRY_LEN)
        .ok_or_else(|| "index table length overflows buffer offset arithmetic".to_string())?;
    let index_end = index_start
        .checked_add(index_len)
        .ok_or_else(|| "index table length overflows buffer offset arithmetic".to_string())?;
    if index_end > bytes.len() {
        return Err(format!(
            "index table (offset {index_start}..{index_end}, {part_count} entrie(s)) exceeds the pack buffer's {} byte(s)",
            bytes.len()
        ));
    }

    struct IndexEntry {
        part_id: u32,
        src_type: u8,
        data_offset: u32,
        data_len: u32,
    }

    let mut entries = Vec::with_capacity(part_count);
    for i in 0..part_count {
        let entry_start = index_start + i * INDEX_ENTRY_LEN;
        let entry = &bytes[entry_start..entry_start + INDEX_ENTRY_LEN];
        entries.push(IndexEntry {
            part_id: u32::from_le_bytes(entry[0..4].try_into().unwrap()),
            src_type: entry[4],
            data_offset: u32::from_le_bytes(entry[5..9].try_into().unwrap()),
            data_len: u32::from_le_bytes(entry[9..13].try_into().unwrap()),
        });
    }

    let mut parts: HashMap<u32, PartData> = HashMap::new();

    for entry in &entries {
        let data_start = entry.data_offset as usize;
        let data_len = entry.data_len as usize;
        let data_end = data_start.checked_add(data_len).ok_or_else(|| {
            format!(
                "part_id {}: data_offset + data_len overflows buffer offset arithmetic",
                entry.part_id
            )
        })?;
        if data_end > bytes.len() {
            return Err(format!(
                "part_id {}: data range {data_start}..{data_end} exceeds the pack buffer's {} byte(s)",
                entry.part_id,
                bytes.len()
            ));
        }
        let part_bytes = &bytes[data_start..data_end];

        let (mut vertices, indices, local_bone_names) = match entry.src_type {
            0 => {
                let loaded = obj_loader::load_obj_bytes(part_bytes).map_err(|e| {
                    format!("part_id {}: failed to parse OBJ bytes: {e}", entry.part_id)
                })?;
                (
                    loaded.vertices,
                    loaded.indices,
                    vec![obj_loader::OBJ_BIND_BONE_NAME.to_string()],
                )
            }
            1 => {
                let loaded = gltf_loader::load_gltf_bytes(part_bytes).map_err(|e| {
                    format!("part_id {}: failed to parse glTF/GLB bytes: {e}", entry.part_id)
                })?;
                skeleton_resolver::contribute_global_joints(
                    &mut master_skeleton,
                    &loaded.local_bone_names,
                    &loaded.local_joint_parents,
                    &loaded.local_joint_bind_poses,
                )
                .map_err(|e| {
                    format!("part_id {}: skeleton contribution failed: {e}", entry.part_id)
                })?;
                (loaded.vertices, loaded.indices, loaded.local_bone_names)
            }
            other => {
                return Err(format!(
                    "part_id {}: invalid src_type {other} (expected 0 = OBJ or 1 = glTF/GLB)",
                    entry.part_id
                ))
            }
        };

        skeleton_resolver::resolve_bone_indices(&mut vertices, &local_bone_names, &master_skeleton)
            .map_err(|e| format!("part_id {}: skeleton resolution failed: {e}", entry.part_id))?;

        if parts
            .insert(entry.part_id, PartData { vertices, indices })
            .is_some()
        {
            return Err(format!(
                "duplicate part_id {} within one Part Pack",
                entry.part_id
            ));
        }
    }

    let part_count = parts.len();

    check_global_joints_complete(&master_skeleton)?;
    let global_joints = finalize_global_joints(master_skeleton)?;

    let registry = Registry {
        parts,
        global_joints,
        skin_tree_cache: std::sync::RwLock::new(HashMap::new()),
        clothing_anchor_cache: std::sync::RwLock::new(HashMap::new()),
    };

    // Matches `init_part_registry_impl`'s existing convention: losing a
    // concurrent `OnceLock::set` race is treated as an ordinary failure,
    // not a crash.
    GLOBAL_REGISTRY
        .set(registry)
        .map_err(|_| "registry already initialized".to_string())?;

    Ok(part_count)
}

/// Generate a character's mesh for the given DNA.
///
/// Full procedural composition, as of this integration:
/// 1. `dna.head_id` and `dna.torso_id` are both resolved against the part
///    registry and merged (`mesh_merge::merge_parts`) into one combined
///    skin buffer.
/// 2. That combined skin is DNA-mutated (`body_mutation::mutate_skin_vertices`,
///    with the scale derived by `clothing_deformer::dna_scale_from_character_dna`).
/// 3. Every id in `dna.equipped_clothing_ids_ptr`/`equipped_clothing_count`
///    is resolved against the registry, fitted to the mutated skin via
///    `clothing_deformer::fit_clothing_to_skin` using this exact
///    (`head_id`, `torso_id`) pair's cached anchors, and merged onto the
///    output. A clothing id that fails to resolve (unknown id, or no
///    anchors cached/buildable for this specific head/torso combination)
///    is logged and skipped rather than failing the whole character — see
///    `Registry::get_or_build_clothing_anchors`'s caveat that clothing
///    anchors are only ever built against one reference body (the actual
///    resolved head/torso of the character being generated), not
///    pre-declared for a fixed set of "supported" body types.
///
/// # Safety
/// `dna` must be a valid, non-null, correctly-aligned pointer to a fully
/// initialized `CharacterDNA` for the duration of this call.
///
/// # Returns
/// A heap-allocated `MeshOutputBuffer*` whose `vertices_ptr`/`indices_ptr`
/// arrays are owned by the caller (see module-level safety contract), or
/// null on any failure: `dna` was null, the registry was never
/// initialized, `dna.head_id`/`dna.torso_id` does not name a loaded part,
/// or the head/torso merge or DNA mutation step failed. A failure to fit
/// or resolve an individual *clothing* item is not fatal to the whole
/// call (see above) and does not by itself cause a null return.
#[no_mangle]
pub extern "C" fn generate_character(dna: *const CharacterDNA) -> *mut MeshOutputBuffer {
    // See `init_part_registry`'s matching comment: always clear first, so
    // a successful call never inherits a stale error from an earlier
    // failing one on this thread.
    clear_last_error();

    if dna.is_null() {
        eprintln!("[anthroforge] generate_character called with null DNA pointer");
        set_last_error("generate_character: dna pointer was null");
        return std::ptr::null_mut();
    }

    let Some(registry) = GLOBAL_REGISTRY.get() else {
        eprintln!("[anthroforge] generate_character called before init_part_registry succeeded");
        set_last_error(
            "generate_character: called before init_part_registry succeeded; the part registry is not initialized",
        );
        return std::ptr::null_mut();
    };

    // SAFETY: caller contract (see doc comment above and module-level
    // safety contract) guarantees `dna` is non-null, aligned, and points
    // at a valid, fully-initialized `CharacterDNA` for this call.
    let dna: &CharacterDNA = unsafe { &*dna };

    let Some(head) = registry.parts.get(&dna.head_id) else {
        eprintln!(
            "[anthroforge] generate_character: no part loaded for head_id {}",
            dna.head_id
        );
        set_last_error(format!(
            "generate_character: no part loaded for head_id {}",
            dna.head_id
        ));
        return std::ptr::null_mut();
    };

    let Some(torso) = registry.parts.get(&dna.torso_id) else {
        eprintln!(
            "[anthroforge] generate_character: no part loaded for torso_id {}",
            dna.torso_id
        );
        set_last_error(format!(
            "generate_character: no part loaded for torso_id {}",
            dna.torso_id
        ));
        return std::ptr::null_mut();
    };

    let (merged_vertices, merged_indices) = match mesh_merge::merge_parts(&[
        (head.vertices.as_slice(), head.indices.as_slice()),
        (torso.vertices.as_slice(), torso.indices.as_slice()),
    ]) {
        Ok(merged) => merged,
        Err(e) => {
            eprintln!("[anthroforge] generate_character: failed to merge head/torso parts: {e}");
            set_last_error(format!(
                "generate_character: failed to merge head/torso parts: {e}"
            ));
            return std::ptr::null_mut();
        }
    };

    let scale = clothing_deformer::dna_scale_from_character_dna(dna);

    let mutated_vertices = match body_mutation::mutate_skin_vertices(&merged_vertices, scale) {
        Ok(vertices) => vertices,
        Err(e) => {
            eprintln!("[anthroforge] generate_character: DNA mutation failed: {e}");
            set_last_error(format!("generate_character: DNA mutation failed: {e}"));
            return std::ptr::null_mut();
        }
    };

    // Resolved the same way `head_id`/`torso_id` are already read from
    // `dna` above — this pointer/count pair already exists on
    // `CharacterDNA`, this is simply the first place that consumes it.
    //
    // SAFETY: caller contract on `generate_character` guarantees `dna` is
    // fully initialized, so `equipped_clothing_ids_ptr` is either null
    // with `equipped_clothing_count == 0`, or valid for reads of
    // `equipped_clothing_count` consecutive `u32` values.
    let equipped_clothing_ids: &[u32] = if dna.equipped_clothing_count == 0 {
        &[]
    } else if dna.equipped_clothing_ids_ptr.is_null() {
        eprintln!(
            "[anthroforge] generate_character: null equipped_clothing_ids_ptr with nonzero \
             equipped_clothing_count {}; treating as no equipped clothing",
            dna.equipped_clothing_count
        );
        &[]
    } else {
        unsafe {
            slice::from_raw_parts(
                dna.equipped_clothing_ids_ptr,
                dna.equipped_clothing_count as usize,
            )
        }
    };

    let mut fitted_clothing: Vec<(Vec<SkinnedVertex>, Vec<u32>)> = Vec::new();

    for &clothing_id in equipped_clothing_ids {
        let Some(clothing_part) = registry.parts.get(&clothing_id) else {
            eprintln!(
                "[anthroforge] generate_character: no part loaded for equipped clothing_id \
                 {clothing_id}; skipping this item"
            );
            continue;
        };

        let anchors =
            match registry.get_or_build_clothing_anchors(dna.head_id, dna.torso_id, clothing_id) {
                Ok(anchors) => anchors,
                Err(e) => {
                    eprintln!(
                        "[anthroforge] generate_character: no clothing anchors for clothing_id \
                         {clothing_id} on (head_id {}, torso_id {}): {e}; skipping this item",
                        dna.head_id, dna.torso_id
                    );
                    continue;
                }
            };

        let mut cloth_vertices = clothing_part.vertices.clone();
        match clothing_deformer::fit_clothing_to_skin(
            &mutated_vertices,
            &mut cloth_vertices,
            &anchors,
            scale,
            CLOTHING_CLEARANCE_EPSILON,
        ) {
            Ok(()) => {
                fitted_clothing.push((cloth_vertices, clothing_part.indices.clone()));
            }
            Err(e) => {
                eprintln!(
                    "[anthroforge] generate_character: failed to fit equipped clothing_id \
                     {clothing_id}: {e}; skipping this item"
                );
            }
        }
    }

    let mut parts_to_merge: Vec<(&[SkinnedVertex], &[u32])> =
        Vec::with_capacity(1 + fitted_clothing.len());
    parts_to_merge.push((mutated_vertices.as_slice(), merged_indices.as_slice()));
    for (cloth_vertices, cloth_indices) in &fitted_clothing {
        parts_to_merge.push((cloth_vertices.as_slice(), cloth_indices.as_slice()));
    }

    let (mut vertices, mut indices) = match mesh_merge::merge_parts(&parts_to_merge) {
        Ok(merged) => merged,
        Err(e) => {
            eprintln!(
                "[anthroforge] generate_character: failed to merge fitted clothing onto body: {e}"
            );
            set_last_error(format!(
                "generate_character: failed to merge fitted clothing onto body: {e}"
            ));
            return std::ptr::null_mut();
        }
    };

    vertices.shrink_to_fit();
    indices.shrink_to_fit();

    let vertices_count = vertices.len() as u32;
    let indices_count = indices.len() as u32;
    let vertices_ptr = vertices.as_mut_ptr();
    let indices_ptr = indices.as_mut_ptr();

    // Transfer ownership of both arrays to the caller. `free_mesh_buffer`
    // reconstructs each `Vec` with `Vec::from_raw_parts(ptr, len, len)` and
    // lets it drop, which is the exact inverse of this `mem::forget`. Both
    // `vertices`/`indices` were `shrink_to_fit()`-ed immediately above so
    // `len == capacity`, matching what `Vec::from_raw_parts` requires.
    std::mem::forget(vertices);
    std::mem::forget(indices);

    let buffer = MeshOutputBuffer {
        vertices_ptr,
        indices_ptr,
        vertices_count,
        indices_count,
    };

    // The buffer struct itself is also caller-owned, for the same reason:
    // `free_mesh_buffer` reconstructs it with `Box::from_raw`.
    Box::into_raw(Box::new(buffer))
}

/// Releases a `MeshOutputBuffer` produced by `generate_character`,
/// including the `vertices_ptr`/`indices_ptr` arrays it owns. Safe to call
/// with `null` (no-op). Must not be called twice on the same pointer, and
/// the pointer must not be used again after this call.
///
/// This export was documented (in every phase's doc comments and safety
/// contract) as the required counterpart to `generate_character`, but was
/// never actually implemented in Phases 1-4 — every `MeshOutputBuffer`
/// `generate_character` ever returned was therefore leaked permanently.
/// Added here as part of the Phase 5 FFI-surface reconciliation, mirroring
/// `free_atlas_buffer`'s existing pattern exactly. See the Phase 5
/// write-up for this flagged as a gap rather than silently patched over.
///
/// # Safety
/// `buffer` must be either null or a pointer previously returned by
/// `generate_character` that has not already been freed. Its
/// `vertices_ptr`/`indices_ptr` fields must be either null, or exactly the
/// pointer/length pair `generate_character` produced for them (untouched
/// by the caller in a way that would violate `Vec::from_raw_parts`'s
/// `len == capacity` requirement).
#[no_mangle]
pub extern "C" fn free_mesh_buffer(buffer: *mut MeshOutputBuffer) {
    if buffer.is_null() {
        return;
    }

    // SAFETY: caller contract guarantees `buffer` was produced by
    // `generate_character` and not yet freed, so reconstructing the `Box`
    // here is valid and takes ownership back from the caller.
    let boxed = unsafe { Box::from_raw(buffer) };
    let MeshOutputBuffer {
        vertices_ptr,
        indices_ptr,
        vertices_count,
        indices_count,
    } = *boxed;

    if !vertices_ptr.is_null() {
        // SAFETY: `generate_character` allocated this via a `Vec` that was
        // `shrink_to_fit()`-ed immediately before `mem::forget`, so
        // `len == capacity == vertices_count`, matching what
        // `Vec::from_raw_parts` requires.
        let reclaimed = unsafe {
            Vec::from_raw_parts(
                vertices_ptr,
                vertices_count as usize,
                vertices_count as usize,
            )
        };
        drop(reclaimed);
    }

    if !indices_ptr.is_null() {
        // SAFETY: same reasoning as above, for the indices array.
        let reclaimed = unsafe {
            Vec::from_raw_parts(indices_ptr, indices_count as usize, indices_count as usize)
        };
        drop(reclaimed);
    }
}

/// Returns the fully-assembled global skeleton — `master_skeleton.json`'s
/// bone hierarchy plus every bone's real bind-pose local transform,
/// contributed across every loaded part (see `skeleton_resolver`) — as a
/// heap-allocated `SkeletonBuffer*` whose `joints_ptr` array is owned by
/// the caller (see module-level safety contract) until released via
/// [`free_skeleton_buffer`].
///
/// A JS caller uses each `FfiJoint`'s `parent_index` (`-1` for the root)
/// plus `translation`/`rotation`/`scale` to build one `THREE.Bone` per
/// joint, `.add()` each onto its parent, and construct `new
/// THREE.Skeleton(bones)` with no second argument —
/// `skeleton.calculateInverses()` runs automatically from that bind-pose
/// hierarchy. This crate deliberately does not also extract or expose
/// each skin's `inverseBindMatrices` accessor: `THREE.Skeleton` computes
/// correct inverse bind matrices itself from the bones' local transforms
/// and parent/child hierarchy, so extracting them separately would be
/// redundant work and a second, unnecessary place for cross-part
/// inconsistency to creep in (see the Phase 4 skeleton-extraction task
/// write-up).
///
/// This data is read-only, global, and per-process — unlike
/// `generate_character`'s per-call output — so call this once, after
/// `init_part_registry`/`init_part_registry_from_pack` succeeds, not once
/// per generated character.
///
/// # Returns
/// A valid `SkeletonBuffer*` on success, or `null` (with
/// `set_last_error(...)`) if the registry has not been initialized yet,
/// or if an internal panic occurred (caught via `catch_unwind`, the same
/// panic-safety guarantee `build_cloth_anchors_for_part`/
/// `free_cloth_anchor_buffer` already provide).
#[no_mangle]
pub extern "C" fn get_skeleton() -> *mut SkeletonBuffer {
    // See `init_part_registry`/`generate_character`'s matching comment:
    // always clear first, so a successful call never inherits a stale
    // error from an earlier failing one on this thread.
    clear_last_error();

    let result = panic::catch_unwind(AssertUnwindSafe(get_skeleton_impl));

    match result {
        Ok(Ok(buffer)) => Box::into_raw(Box::new(buffer)),
        Ok(Err(message)) => {
            eprintln!("[anthroforge] get_skeleton failed: {message}");
            set_last_error(format!("get_skeleton failed: {message}"));
            std::ptr::null_mut()
        }
        Err(_) => {
            eprintln!("[anthroforge] get_skeleton: internal panic suppressed at FFI boundary");
            set_last_error("get_skeleton: internal panic suppressed at FFI boundary");
            std::ptr::null_mut()
        }
    }
}

fn get_skeleton_impl() -> Result<SkeletonBuffer, String> {
    let Some(registry) = GLOBAL_REGISTRY.get() else {
        return Err(
            "called before init_part_registry succeeded; the part registry is not initialized"
                .to_string(),
        );
    };

    let mut joints: Vec<FfiJoint> = Vec::with_capacity(registry.global_joints.len());
    for joint in &registry.global_joints {
        joints.push(FfiJoint {
            parent_index: joint.parent.map(|p| p as i32).unwrap_or(-1),
            translation: joint.bind_pose.translation,
            rotation: joint.bind_pose.rotation,
            scale: joint.bind_pose.scale,
        });
    }

    // Ensure capacity == len so `free_skeleton_buffer` can reconstruct
    // the Vec exactly via `Vec::from_raw_parts`, matching
    // `generate_character`/`build_cloth_anchors_for_part_impl`'s own
    // pre-leak `shrink_to_fit()` step.
    joints.shrink_to_fit();
    let joint_count = joints.len() as u32;
    let joints_ptr = joints.as_mut_ptr();
    std::mem::forget(joints);

    Ok(SkeletonBuffer {
        joints_ptr,
        joint_count,
    })
}

/// Releases a [`SkeletonBuffer`] produced by [`get_skeleton`]. Safe to
/// call with `null` (no-op). Must not be called twice on the same
/// pointer, and the pointer must not be used again after this call.
///
/// # Safety
/// `buffer` must be either null or a pointer previously returned by
/// [`get_skeleton`] that has not already been freed.
#[no_mangle]
pub extern "C" fn free_skeleton_buffer(buffer: *mut SkeletonBuffer) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        if buffer.is_null() {
            return;
        }

        // SAFETY: caller contract guarantees `buffer` was produced by
        // `get_skeleton` and not yet freed, so reconstructing the `Box`
        // here is valid and takes ownership back from the caller.
        let boxed = unsafe { Box::from_raw(buffer) };
        let SkeletonBuffer {
            joints_ptr,
            joint_count,
        } = *boxed;

        if !joints_ptr.is_null() {
            // SAFETY: `get_skeleton_impl` allocated this via a `Vec` that
            // was `shrink_to_fit()`-ed immediately before `mem::forget`,
            // so `len == capacity == joint_count`, matching what
            // `Vec::from_raw_parts` requires.
            let reclaimed = unsafe {
                Vec::from_raw_parts(joints_ptr, joint_count as usize, joint_count as usize)
            };
            drop(reclaimed);
        }
    }));

    if result.is_err() {
        eprintln!("[anthroforge] free_skeleton_buffer: internal panic suppressed at FFI boundary");
    }
}

// ============================================================================
// Minimal host-memory interop for a raw (non-wasm-bindgen) JS harness.
// ============================================================================
//
// wasm32-unknown-unknown exports no allocator to the host by default. Every
// existing export in this crate (`init_part_registry`, `generate_character`,
// ...) takes a pointer *into* wasm linear memory as an argument, but nothing
// in the crate previously gave a JS caller a way to obtain a valid, writable
// address in that memory to write the `asset_dir` C string or a
// `CharacterDNA` into before calling them. Without this, a raw
// `WebAssembly.instantiate` harness has no legal address to pass — this is
// the standard minimal "expose the allocator" pair for that situation, nothing
// wasm-bindgen-specific about it. Purely additive: does not change any
// existing export's signature or behavior.
#[no_mangle]
pub extern "C" fn wasm_alloc(size: usize) -> *mut u8 {
    let mut buf: Vec<u8> = Vec::with_capacity(size);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// # Safety
/// `ptr` must be a pointer previously returned by `wasm_alloc` called with
/// this exact `size`, and must not have been freed already.
#[no_mangle]
pub extern "C" fn wasm_dealloc(ptr: *mut u8, size: usize) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: caller contract above.
    unsafe {
        drop(Vec::from_raw_parts(ptr, 0, size));
    }
}

// ============================================================================
// getrandom "custom" backend (Phase 1 feasibility-harness fix).
// ============================================================================
//
// Cargo.toml switched `getrandom` (pulled transitively via
// `tobj -> ahash -> getrandom`, needed by `obj_loader::load_obj_bytes`'s
// call into `tobj::load_obj_buf`) from the "wasm_js" backend to "custom".
// "wasm_js" implements its entropy source using wasm-bindgen internally,
// which injects `__wbindgen_*` imports into the compiled .wasm that a raw
// `WebAssembly.instantiate` harness (harness.mjs) has no way to satisfy
// without reimplementing an unstable, non-trivial slice of wasm-bindgen's
// own ABI — confirmed as the root cause of a real LinkError naming
// `__wbindgen_object_drop_ref` when this was still "wasm_js".
//
// "custom" requires exactly this: a function named `__getrandom_v03_custom`
// with this exact signature, defined anywhere in the dependency graph
// (verified against getrandom 0.3.4's real docs, the version pinned in
// Cargo.lock — this signature is specific to the 0.3.x line and differs
// from 0.2's `register_custom_getrandom!` macro-based API). It forwards
// to one plain `extern "C"` import that the JS host (harness.mjs) must
// supply — no wasm-bindgen involved on either side.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "anthroforge_host")]
extern "C" {
    /// Fills `len` bytes at `ptr` (a valid address in this module's own
    /// linear memory) with random bytes. Implemented host-side in
    /// harness.mjs via Node's `crypto.randomFillSync`.
    fn fill_random(ptr: *mut u8, len: usize);
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // SAFETY: `dest` is the same pointer getrandom's caller (here,
    // ahash, via tobj) guarantees is valid for writes of `len` bytes;
    // we pass it straight through to the host import unchanged.
    fill_random(dest, len);
    Ok(())
}

/// FFI entry point for cache prewarming. On native targets, spawns the
/// actual work on a background thread and returns immediately — callers
/// must not assume the cache is populated right after this call returns,
/// only that it will be soon. **On `wasm32-unknown-unknown`, this runs
/// synchronously instead** (see the wasm32 branch in this function's body
/// for why) — callers on that target should assume the cache IS populated
/// by the time this call returns, and should also expect this call to
/// take as long as building the anchors themselves does, unlike the
/// fire-and-forget native behavior.
///
/// Calling this is entirely optional for a game integration — every code
/// path in `Registry::get_or_build_clothing_anchors` already works
/// correctly without it ever being called. This is purely a
/// latency-hiding optimization for known combinations (e.g. from a spawn
/// table or character-select roster), not a required step.
///
/// Safe by construction against the FFI-panic-abort concern documented
/// elsewhere in this crate **on the native target only**: there, this
/// spawns a *new* std thread, and a panic inside a spawned thread (under
/// this crate's `panic = "unwind"` profile) only unwinds and terminates
/// that thread — it does not cross any `extern "C"` boundary and does not
/// abort the process. Worst case, a malformed prewarm request quietly
/// fails to populate the cache for that batch, and a later real
/// `generate_character` call for that combination simply builds it
/// lazily as normal, exactly as if prewarming had never been called. On
/// wasm32 this reasoning does not apply — this target has no real thread
/// support to spawn in the first place (a bare `std::thread::spawn` here
/// would panic, and this crate's own wasm32 finding is that `panic =
/// "unwind"` does not actually unwind on this target, so that panic would
/// trap the whole module) — which is exactly why the wasm32 branch below
/// runs synchronously instead of spawning at all.
///
/// # Safety
/// - `clothing_ids_ptr` must be valid for reads of `clothing_count`
///   consecutive `u32` values when `clothing_count != 0`.
/// - `clothing_ids_ptr` may be null only if `clothing_count == 0`.
#[no_mangle]
pub extern "C" fn anthroforge_prewarm_clothing(
    head_id: u32,
    torso_id: u32,
    clothing_ids_ptr: *const u32,
    clothing_count: u32,
) {
    if clothing_count == 0 {
        return;
    }
    if clothing_ids_ptr.is_null() {
        eprintln!(
            "[anthroforge] anthroforge_prewarm_clothing: null clothing_ids_ptr with nonzero \
             clothing_count {clothing_count}; ignoring"
        );
        return;
    }

    let Some(registry) = GLOBAL_REGISTRY.get() else {
        eprintln!("[anthroforge] anthroforge_prewarm_clothing called before init_part_registry succeeded");
        return;
    };

    // Copy the ids into an owned Vec *before* spawning: the raw pointer's
    // lifetime is only guaranteed for the duration of this call, not for
    // however long the background thread takes to run.
    //
    // SAFETY: caller contract above guarantees `clothing_ids_ptr` is valid
    // for reads of `clothing_count` consecutive `u32` values here.
    let owned_ids: Vec<u32> =
        unsafe { slice::from_raw_parts(clothing_ids_ptr, clothing_count as usize) }.to_vec();

    // wasm32-unknown-unknown has no native OS thread support under this
    // build (same root cause already fixed once in `texture_atlas.rs`'s
    // `generate_runtime_atlas_impl`) — `std::thread::spawn`'s internal
    // spawn call would panic, and per this crate's own wasm32 finding
    // (`panic = "unwind"` does not actually unwind on this target), that
    // panic traps the whole module rather than staying contained to "a
    // background thread that failed," as this function's own doc comment
    // above assumes. Since this call is documented as purely an optional
    // latency-hiding step — every code path in
    // `Registry::get_or_build_clothing_anchors` already works correctly
    // without it ever being called — the correct wasm32 behavior is to run
    // the same work synchronously on the caller's thread instead of
    // skipping it silently: slower (blocks the caller briefly), but still
    // populates the cache, and does not trap.
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::spawn(move || {
        prewarm_clothing_anchors_impl(registry, head_id, torso_id, &owned_ids);
    });
    #[cfg(target_arch = "wasm32")]
    prewarm_clothing_anchors_impl(registry, head_id, torso_id, &owned_ids);
}

// ============================================================================
// Internal implementation (pure Result-based, no FFI concerns).
// ============================================================================

fn init_part_registry_impl(asset_dir: *const c_char) -> Result<usize, String> {
    if asset_dir.is_null() {
        return Err("asset_dir pointer was null".to_string());
    }

    // SAFETY: caller contract guarantees `asset_dir` is a valid, non-null,
    // NUL-terminated C string for the duration of this call.
    let c_str = unsafe { CStr::from_ptr(asset_dir) };
    let asset_dir_str = c_str
        .to_str()
        .map_err(|e| format!("asset_dir was not valid UTF-8: {e}"))?;
    let asset_dir = PathBuf::from(asset_dir_str);

    if !asset_dir.is_dir() {
        return Err(format!(
            "asset_dir '{}' does not exist or is not a directory",
            asset_dir.display()
        ));
    }

    let master_skeleton_path = asset_dir.join("master_skeleton.json");
    let mut master_skeleton = skeleton_resolver::load_master_skeleton(&master_skeleton_path)
        .map_err(|e| e.to_string())?;

    let entries = std::fs::read_dir(&asset_dir)
        .map_err(|e| format!("failed to read asset_dir '{}': {e}", asset_dir.display()))?;

    let mut parts: HashMap<u32, PartData> = HashMap::new();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("[anthroforge] skipping unreadable directory entry: {e}");
                continue;
            }
        };
        let path = entry.path();

        let Some(part_kind) = classify_part_file(&path) else {
            continue;
        };

        let part_id = match parse_part_id(&path) {
            Some(id) => id,
            None => {
                eprintln!(
                    "[anthroforge] skipping '{}': filename does not start with a numeric part id",
                    path.display()
                );
                continue;
            }
        };

        // Both branches converge on the same shape (vertices still using
        // *local* bone indices, plus the local bone-name list those local
        // indices are keyed against) so exactly one
        // `skeleton_resolver::resolve_bone_indices` call below handles
        // both loaders identically — an `.obj` part is not a special case
        // as far as skeleton resolution is concerned, it just always has
        // the same one-entry local bone list.
        let (mut vertices, indices, local_bone_names) = match part_kind {
            PartFileKind::Gltf => match gltf_loader::load_gltf_file(&path) {
                Ok(loaded) => {
                    if let Err(e) = skeleton_resolver::contribute_global_joints(
                        &mut master_skeleton,
                        &loaded.local_bone_names,
                        &loaded.local_joint_parents,
                        &loaded.local_joint_bind_poses,
                    ) {
                        eprintln!(
                            "[anthroforge] skipping '{}': skeleton contribution failed: {e}",
                            path.display()
                        );
                        continue;
                    }
                    (loaded.vertices, loaded.indices, loaded.local_bone_names)
                }
                Err(e) => {
                    eprintln!("[anthroforge] skipping '{}': {e}", path.display());
                    continue;
                }
            },
            PartFileKind::Obj => match obj_loader::load_obj_file(&path) {
                Ok(loaded) => (
                    loaded.vertices,
                    loaded.indices,
                    vec![obj_loader::OBJ_BIND_BONE_NAME.to_string()],
                ),
                Err(e) => {
                    eprintln!("[anthroforge] skipping '{}': {e}", path.display());
                    continue;
                }
            },
        };

        if let Err(e) = skeleton_resolver::resolve_bone_indices(
            &mut vertices,
            &local_bone_names,
            &master_skeleton,
        ) {
            eprintln!(
                "[anthroforge] skipping '{}': skeleton resolution failed: {e}",
                path.display()
            );
            continue;
        }

        if parts
            .insert(part_id, PartData { vertices, indices })
            .is_some()
        {
            eprintln!(
                "[anthroforge] warning: part id {part_id} loaded from '{}' overwrote a previously loaded part with the same id",
                path.display()
            );
        }
    }

    if parts.is_empty() {
        return Err(format!(
            "no valid .gltf/.glb/.obj parts were loaded from '{}'",
            asset_dir.display()
        ));
    }

    let part_count = parts.len();

    check_global_joints_complete(&master_skeleton)?;
    let global_joints = finalize_global_joints(master_skeleton)?;

    GLOBAL_REGISTRY
        .set(Registry {
            parts,
            global_joints,
            skin_tree_cache: std::sync::RwLock::new(HashMap::new()),
            clothing_anchor_cache: std::sync::RwLock::new(HashMap::new()),
        })
        .map_err(|_| "init_part_registry was already called; registry is immutable after the first successful init".to_string())?;

    Ok(part_count)
}

/// Which loader a modular part file's extension routes to. `.gltf`/`.glb`
/// go through `gltf_loader` (full skin data); `.obj` goes through
/// `obj_loader` (Phase 2's rigid-bind fallback for parts with no skin at
/// all). Any other extension (or non-file entry) is not a modular part
/// and is skipped by the caller.
enum PartFileKind {
    Gltf,
    Obj,
}

fn classify_part_file(path: &Path) -> Option<PartFileKind> {
    if !path.is_file() {
        return None;
    }
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("gltf") || ext.eq_ignore_ascii_case("glb") => {
            Some(PartFileKind::Gltf)
        }
        Some(ext) if ext.eq_ignore_ascii_case("obj") => Some(PartFileKind::Obj),
        _ => None,
    }
}

/// Part ids are taken from the leading run of ASCII digits in the file
/// stem, e.g. `1001_head_male.glb` -> `1001`. Returns `None` (skip, don't
/// error the whole init out) if the filename doesn't start with a digit.
fn parse_part_id(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

// ============================================================================
// Tests — Phase 5 FFI error propagation.
//
// NOTE ON GLOBAL_REGISTRY: `GLOBAL_REGISTRY` is a process-wide `OnceLock`,
// shared by every test in this binary (`cargo test` runs all `#[test]`
// functions from this crate in one process). It can only ever be
// successfully initialized once, by whichever test gets there first, and
// stays initialized for the rest of the run. `generate_character_*` tests
// below therefore only test the states that are true *regardless* of
// whether some other test has already initialized it (null DNA is checked
// before the registry is even consulted), or explicitly branch on
// `GLOBAL_REGISTRY.get()` to test whichever of the two "before/after init"
// error paths actually applies at the moment they run — this file's own
// `init_part_registry`-calling test is the only one intentionally
// exercising the initialized side, immediately after which it re-asserts
// the crate's own invariant (a part was in fact loaded).
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn last_error_message() -> Option<String> {
        let ptr = anthroforge_last_error();
        if ptr.is_null() {
            None
        } else {
            // SAFETY: `anthroforge_last_error` guarantees the returned
            // pointer, if non-null, is a valid, NUL-terminated C string
            // for as long as this thread makes no further library call —
            // true here, since we copy it out immediately.
            Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
        }
    }

    #[test]
    fn generate_character_null_dna_sets_specific_last_error() {
        let result = generate_character(std::ptr::null());
        assert!(result.is_null());

        let message = last_error_message().expect("null-dna failure must set a last error");
        assert!(
            message.contains("dna pointer was null"),
            "expected a message about the null dna pointer, got: {message}"
        );
    }

    #[test]
    fn generate_character_before_registry_init_sets_specific_last_error() {
        // Only meaningful if some other test hasn't already initialized
        // the shared, process-wide registry first; if it has, this test
        // instead falls through to asserting the *other* failure path
        // (unknown head_id) still sets a specific, non-generic message —
        // either way, `generate_character` must never fail silently.
        let dna = CharacterDNA {
            seed: 0,
            height_modifier: 0.0,
            weight_modifier: 0.0,
            head_id: u32::MAX, // never a real, loaded part id in any test asset
            torso_id: 0,
            equipped_clothing_ids_ptr: std::ptr::null(),
            equipped_clothing_count: 0,
        };

        let result = generate_character(&dna as *const CharacterDNA);
        assert!(result.is_null());

        let message = last_error_message().expect("failure must set a last error");
        if GLOBAL_REGISTRY.get().is_none() {
            assert!(
                message.contains("before init_part_registry"),
                "expected a message about the registry not being initialized, got: {message}"
            );
        } else {
            assert!(
                message.contains("no part loaded for head_id"),
                "expected a message about the missing head_id, got: {message}"
            );
        }
    }

    #[test]
    fn last_error_does_not_leak_stale_message_into_next_failure() {
        // First failure: null-dna path.
        assert!(generate_character(std::ptr::null()).is_null());
        let first_message = last_error_message().expect("first failure must set a last error");
        assert!(first_message.contains("dna pointer was null"));

        // Second, unrelated failure: bad (non-UTF8-independent, simply
        // nonexistent) asset_dir path via init_part_registry. This must
        // *replace* the first message, not append to or coexist with it.
        let bad_path = CString::new("/this/path/does/not/exist/anthroforge-test").unwrap();
        assert!(!init_part_registry(bad_path.as_ptr()));

        let second_message = last_error_message().expect("second failure must set a last error");
        assert!(
            !second_message.contains("dna pointer was null"),
            "stale message from the first failure leaked into the second: {second_message}"
        );
        assert!(
            second_message.contains("does not exist or is not a directory"),
            "expected a message about the missing asset_dir, got: {second_message}"
        );
    }

    #[test]
    fn init_part_registry_null_asset_dir_sets_last_error() {
        assert!(!init_part_registry(std::ptr::null()));
        let message = last_error_message().expect("null asset_dir failure must set a last error");
        assert!(message.contains("asset_dir pointer was null"));
    }

    /// End-to-end: a *successful* `init_part_registry` + `generate_character`
    /// call sequence must leave `anthroforge_last_error()` reporting `null`
    /// — proving the mechanism doesn't leak a stale error from an earlier
    /// failing call (in this same test) into a later successful one, per
    /// the assignment's requirement (b). Uses a real temp directory with a
    /// minimal, valid `.obj` part and `master_skeleton.json`, since
    /// `GLOBAL_REGISTRY` can only be initialized once per process — if some
    /// other test already initialized it (from a different directory),
    /// this test still validates the "successful call clears any stale
    /// error" property using whatever part id that earlier init actually
    /// loaded, by first inducing a real failure, then a real success.
    #[test]
    fn successful_call_after_failure_clears_last_error() {
        // Step 1: induce a real failure and confirm it's recorded.
        assert!(generate_character(std::ptr::null()).is_null());
        assert!(
            last_error_message().is_some(),
            "precondition: a failure must set a last error before we can test it clearing"
        );

        // Step 2: ensure the registry is initialized (idempotently safe:
        // if another test already won the race, this attempt itself fails
        // with "already called", which is fine — we only need `Some`
        // registry state to exist afterward, not that *this* call is the
        // one that succeeded).
        if GLOBAL_REGISTRY.get().is_none() {
            let dir = make_temp_asset_dir();
            let dir_cstring = CString::new(dir.to_str().unwrap()).unwrap();
            let _ = init_part_registry(dir_cstring.as_ptr());
        }

        let Some(registry) = GLOBAL_REGISTRY.get() else {
            // Extremely unlikely (would mean our own init attempt above
            // failed for a reason other than "already initialized" and no
            // other test initialized it either) — nothing further to
            // assert against a registry that was never populated.
            return;
        };

        let Some(&any_loaded_head_id) = registry.parts.keys().next() else {
            return;
        };

        let dna = CharacterDNA {
            seed: 42,
            // Must be finite and > 0.0: as of the Phase 6 integration,
            // `generate_character` feeds these through
            // `clothing_deformer::dna_scale_from_character_dna` into
            // `body_mutation::mutate_skin_vertices`, which rejects a
            // non-positive scale component. `0.0` (fine back when these
            // fields were write-only placeholders) is no longer valid here.
            height_modifier: 1.0,
            weight_modifier: 1.0,
            head_id: any_loaded_head_id,
            // Reuses the same loaded part as both head and torso: as of
            // the Phase 6 integration `torso_id` is resolved against the
            // registry exactly like `head_id` (previously `0` was fine
            // here since `torso_id` was ignored entirely). This test only
            // cares that generation succeeds for ids known to be loaded,
            // not that head and torso are anatomically distinct parts.
            torso_id: any_loaded_head_id,
            equipped_clothing_ids_ptr: std::ptr::null(),
            equipped_clothing_count: 0,
        };
        let buffer = generate_character(&dna as *const CharacterDNA);
        assert!(
            !buffer.is_null(),
            "generate_character should succeed for a head_id known to be loaded"
        );

        assert!(
            last_error_message().is_none(),
            "a successful generate_character call must clear any previously-recorded error"
        );

        free_mesh_buffer(buffer);
    }

    /// Builds a temp directory containing a minimal, valid `master_skeleton.json`
    /// and a single numbered `.obj` part, suitable for a real, successful
    /// `init_part_registry` call.
    fn make_temp_asset_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "anthroforge_test_assets_{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("failed to create temp asset dir");

        std::fs::write(
            dir.join("master_skeleton.json"),
            r#"{"root": 0}"#,
        )
        .expect("failed to write master_skeleton.json");

        // A single triangle, rigidly bound to "root" by obj_loader.
        std::fs::write(
            dir.join("9001_test_part.obj"),
            "v 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 0.0 1.0 0.0\nf 1 2 3\n",
        )
        .expect("failed to write test .obj part");

        dir
    }

    // ========================================================================
    // Tests — two-tier clothing anchor cache (`Registry::get_or_build_*`).
    //
    // Same `GLOBAL_REGISTRY`-is-process-wide constraint applies here as in
    // the tests above: `registry_with_test_clothing_parts` attempts its own
    // init (idempotently safe), then only proceeds if the *specific*
    // head/torso/clothing ids it needs actually ended up loaded — which is
    // guaranteed unless some other test in this file already won the
    // registry-init race with a different asset dir. If that happens these
    // tests are no-ops rather than false failures, matching this file's
    // existing convention for `GLOBAL_REGISTRY`-dependent tests.
    // ========================================================================

    /// Builds a temp directory containing a `master_skeleton.json` plus
    /// four minimal `.obj` parts — a head, a torso, and two distinct
    /// clothing items — at fixed, distinguishable ids. Used by the
    /// clothing-anchor-cache tests below, which need more structure than
    /// the single part `make_temp_asset_dir` provides.
    fn make_temp_asset_dir_with_clothing() -> (PathBuf, u32, u32, u32, u32) {
        const HEAD_ID: u32 = 20001;
        const TORSO_ID: u32 = 20002;
        const CLOTHING_A_ID: u32 = 20003;
        const CLOTHING_B_ID: u32 = 20004;

        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "anthroforge_test_assets_clothing_{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("failed to create temp asset dir");

        std::fs::write(dir.join("master_skeleton.json"), r#"{"root": 0}"#)
            .expect("failed to write master_skeleton.json");

        let triangle = "v 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 0.0 1.0 0.0\nf 1 2 3\n";
        std::fs::write(dir.join(format!("{HEAD_ID}_head.obj")), triangle)
            .expect("failed to write head part");
        std::fs::write(dir.join(format!("{TORSO_ID}_torso.obj")), triangle)
            .expect("failed to write torso part");
        std::fs::write(dir.join(format!("{CLOTHING_A_ID}_clothing_a.obj")), triangle)
            .expect("failed to write clothing part a");
        std::fs::write(dir.join(format!("{CLOTHING_B_ID}_clothing_b.obj")), triangle)
            .expect("failed to write clothing part b");

        (dir, HEAD_ID, TORSO_ID, CLOTHING_A_ID, CLOTHING_B_ID)
    }

    /// Ensures the global registry is initialized (idempotently safe, same
    /// convention as `successful_call_after_failure_clears_last_error`),
    /// then returns the registry plus the fixed head/torso/clothing ids
    /// from `make_temp_asset_dir_with_clothing` — but only if those exact
    /// ids ended up loaded (either this call's own init won the race, or
    /// no other test in this file initializes the registry with a
    /// conflicting asset dir first). Returns `None` otherwise, since the
    /// happy-path tests below need this specific part structure to run.
    fn registry_with_test_clothing_parts() -> Option<(&'static Registry, u32, u32, u32, u32)> {
        let (dir, head_id, torso_id, clothing_a, clothing_b) = make_temp_asset_dir_with_clothing();
        if GLOBAL_REGISTRY.get().is_none() {
            let dir_cstring = CString::new(dir.to_str().unwrap()).unwrap();
            let _ = init_part_registry(dir_cstring.as_ptr());
        }
        let registry = GLOBAL_REGISTRY.get()?;
        if registry.parts.contains_key(&head_id)
            && registry.parts.contains_key(&torso_id)
            && registry.parts.contains_key(&clothing_a)
            && registry.parts.contains_key(&clothing_b)
        {
            Some((registry, head_id, torso_id, clothing_a, clothing_b))
        } else {
            None
        }
    }

    /// Two different clothing items fitted to the *same* (head, torso)
    /// pair must hit the same cached skin-tree `Arc` rather than each
    /// triggering their own tree rebuild.
    #[test]
    fn clothing_anchor_cache_shares_tree_across_clothing_items() {
        let Some((registry, head_id, torso_id, clothing_a, clothing_b)) =
            registry_with_test_clothing_parts()
        else {
            return;
        };

        let tree_before = registry
            .get_or_build_skin_tree(head_id, torso_id)
            .expect("skin tree should build for a valid head/torso pair");

        registry
            .get_or_build_clothing_anchors(head_id, torso_id, clothing_a)
            .expect("anchors should build for clothing_a");
        registry
            .get_or_build_clothing_anchors(head_id, torso_id, clothing_b)
            .expect("anchors should build for clothing_b");

        let tree_after = registry
            .get_or_build_skin_tree(head_id, torso_id)
            .expect("skin tree should still be cached");

        assert!(
            std::sync::Arc::ptr_eq(&tree_before, &tree_after),
            "both clothing_anchors calls should have hit the same cached skin tree, not rebuilt one"
        );
    }

    /// A repeat call for the exact same (head, torso, clothing) combination
    /// must return the cached `Arc`, not rebuild.
    #[test]
    fn clothing_anchor_cache_repeat_call_hits_cache() {
        let Some((registry, head_id, torso_id, clothing_a, _clothing_b)) =
            registry_with_test_clothing_parts()
        else {
            return;
        };

        let first = registry
            .get_or_build_clothing_anchors(head_id, torso_id, clothing_a)
            .expect("first call should succeed");
        let second = registry
            .get_or_build_clothing_anchors(head_id, torso_id, clothing_a)
            .expect("second call should succeed");

        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "repeat call for the same combination must hit the cache, not rebuild"
        );
    }

    /// Two distinct bodies (different (head_id, torso_id) keys — obtained
    /// here by swapping which fixed part plays which role) must not share
    /// a cached tree or anchor entry.
    #[test]
    fn clothing_anchor_cache_distinguishes_different_bodies() {
        let Some((registry, head_id, torso_id, clothing_a, _clothing_b)) =
            registry_with_test_clothing_parts()
        else {
            return;
        };

        let anchors_body_1 = registry
            .get_or_build_clothing_anchors(head_id, torso_id, clothing_a)
            .expect("anchors should build for body 1");
        let anchors_body_2 = registry
            .get_or_build_clothing_anchors(torso_id, head_id, clothing_a)
            .expect("anchors should build for body 2 (swapped head/torso)");

        let tree_body_1 = registry
            .get_or_build_skin_tree(head_id, torso_id)
            .expect("body 1 tree should be cached");
        let tree_body_2 = registry
            .get_or_build_skin_tree(torso_id, head_id)
            .expect("body 2 tree should be cached");

        assert!(
            !std::sync::Arc::ptr_eq(&tree_body_1, &tree_body_2),
            "two distinct (head, torso) keys must not share a cached tree entry"
        );
        assert!(
            !std::sync::Arc::ptr_eq(&anchors_body_1, &anchors_body_2),
            "two distinct (head, torso) keys must not share a cached anchor entry"
        );
    }

    /// `head_id`/`torso_id`/`clothing_id` resolution failures must be
    /// resolved (and returned as the matching typed error) before any
    /// building — and, being a resolution failure, never cached.
    #[test]
    fn clothing_anchor_cache_unresolvable_ids_error() {
        let Some((registry, head_id, torso_id, clothing_a, _clothing_b)) =
            registry_with_test_clothing_parts()
        else {
            return;
        };
        const BOGUS_ID: u32 = u32::MAX;

        assert!(matches!(
            registry.get_or_build_clothing_anchors(BOGUS_ID, torso_id, clothing_a),
            Err(ClothingAnchorError::UnknownHeadId(id)) if id == BOGUS_ID
        ));
        assert!(matches!(
            registry.get_or_build_clothing_anchors(head_id, BOGUS_ID, clothing_a),
            Err(ClothingAnchorError::UnknownTorsoId(id)) if id == BOGUS_ID
        ));
        assert!(matches!(
            registry.get_or_build_clothing_anchors(head_id, torso_id, BOGUS_ID),
            Err(ClothingAnchorError::UnknownClothingId(id)) if id == BOGUS_ID
        ));
    }

    /// `prewarm_clothing_anchors_impl` must populate the cache such that a
    /// subsequent direct `get_or_build_clothing_anchors` call returns the
    /// already-cached `Arc` instead of rebuilding.
    #[test]
    fn prewarm_clothing_anchors_impl_populates_cache() {
        let Some((registry, head_id, torso_id, clothing_a, clothing_b)) =
            registry_with_test_clothing_parts()
        else {
            return;
        };

        prewarm_clothing_anchors_impl(registry, head_id, torso_id, &[clothing_a, clothing_b]);

        let cached = {
            let cache = registry.clothing_anchor_cache.read().unwrap();
            std::sync::Arc::clone(
                cache
                    .get(&(head_id, torso_id, clothing_a))
                    .expect("prewarm should have populated the cache for clothing_a"),
            )
        };

        let direct = registry
            .get_or_build_clothing_anchors(head_id, torso_id, clothing_a)
            .expect("direct call after prewarm should succeed");

        assert!(
            std::sync::Arc::ptr_eq(&cached, &direct),
            "a direct call after prewarming should return the already-cached Arc, not rebuild"
        );
    }

    // ========================================================================
    // Tests — Phase 6 final integration (`generate_character` composing
    // head + torso + DNA mutation + fitted equipped clothing).
    //
    // Same `GLOBAL_REGISTRY`-is-process-wide constraint applies here as
    // everywhere else in this file: `registry_with_test_clothing_parts`
    // only proceeds if the specific fixed head/torso/clothing ids it needs
    // actually ended up loaded, and is a no-op otherwise.
    // ========================================================================

    /// End-to-end: `generate_character` with one equipped clothing item
    /// must return a buffer whose vertex count is the sum of the head,
    /// torso, and clothing part's individual vertex counts, and at least
    /// one clothing vertex in the output must have actually moved from its
    /// original bind-pose position — proof the DNA-mutation + clothing-fit
    /// steps really ran, not a silent no-op.
    #[test]
    fn generate_character_composes_head_torso_and_fitted_clothing() {
        let Some((registry, head_id, torso_id, clothing_a, _clothing_b)) =
            registry_with_test_clothing_parts()
        else {
            return;
        };

        let expected_vertex_count = registry.parts[&head_id].vertices.len()
            + registry.parts[&torso_id].vertices.len()
            + registry.parts[&clothing_a].vertices.len();

        // Bind-pose clothing positions, captured before generation, to
        // compare the fitted output against.
        let original_clothing_positions: Vec<[f32; 3]> = registry.parts[&clothing_a]
            .vertices
            .iter()
            .map(|v| v.position)
            .collect();

        let equipped_ids = [clothing_a];
        // Deliberately non-uniform and non-identity so both the DNA
        // mutation and the clothing delta-morph step actually displace
        // vertices instead of leaving everything at its bind-pose spot.
        let dna = CharacterDNA {
            seed: 7,
            height_modifier: 2.0,
            weight_modifier: 1.5,
            head_id,
            torso_id,
            equipped_clothing_ids_ptr: equipped_ids.as_ptr(),
            equipped_clothing_count: equipped_ids.len() as u32,
        };

        let buffer_ptr = generate_character(&dna as *const CharacterDNA);
        assert!(
            !buffer_ptr.is_null(),
            "generate_character should succeed for valid head/torso/clothing ids"
        );

        // SAFETY: `buffer_ptr` was just returned by `generate_character`
        // and has not been freed yet.
        let buffer = unsafe { &*buffer_ptr };
        assert_eq!(
            buffer.vertices_count as usize, expected_vertex_count,
            "output vertex count should be the sum of head + torso + clothing vertex counts"
        );

        // SAFETY: `vertices_ptr`/`vertices_count` are exactly the pair
        // `generate_character` just produced, read-only, before freeing.
        let output_vertices = unsafe {
            slice::from_raw_parts(buffer.vertices_ptr, buffer.vertices_count as usize)
        };

        // The equipped clothing item's vertices were appended last, after
        // the merged (head + torso) body vertices, by `generate_character`.
        let clothing_start = output_vertices.len() - original_clothing_positions.len();
        let fitted_clothing_positions = &output_vertices[clothing_start..];

        let any_moved = fitted_clothing_positions
            .iter()
            .zip(original_clothing_positions.iter())
            .any(|(fitted, original)| {
                let dx = fitted.position[0] - original[0];
                let dy = fitted.position[1] - original[1];
                let dz = fitted.position[2] - original[2];
                (dx * dx + dy * dy + dz * dz).sqrt() > 1e-4
            });
        assert!(
            any_moved,
            "at least one fitted clothing vertex should differ from its bind-pose position"
        );

        free_mesh_buffer(buffer_ptr);
    }

    /// A valid `head_id` combined with a `torso_id` that names no loaded
    /// part must return null with a last-error message specifically
    /// naming the torso failure (not a generic or head-shaped message).
    #[test]
    fn generate_character_unknown_torso_id_sets_specific_last_error() {
        let Some((_registry, head_id, _torso_id, _clothing_a, _clothing_b)) =
            registry_with_test_clothing_parts()
        else {
            return;
        };
        const BOGUS_TORSO_ID: u32 = u32::MAX;

        let dna = CharacterDNA {
            seed: 0,
            height_modifier: 1.0,
            weight_modifier: 1.0,
            head_id,
            torso_id: BOGUS_TORSO_ID,
            equipped_clothing_ids_ptr: std::ptr::null(),
            equipped_clothing_count: 0,
        };

        let result = generate_character(&dna as *const CharacterDNA);
        assert!(result.is_null());

        let message =
            last_error_message().expect("unknown torso_id failure must set a last error");
        assert!(
            message.contains("no part loaded for torso_id"),
            "expected a message specifically naming the missing torso_id, got: {message}"
        );
    }

    // ========================================================================
    // Tests — concurrency stress test for the clothing-anchor cache tier
    // and `generate_character` under real, multi-threaded OS scheduling.
    //
    // Same `GLOBAL_REGISTRY`-is-process-wide constraint applies here as
    // everywhere else in this file: `registry_with_stress_test_parts` only
    // proceeds if its specific fixed ids actually ended up loaded, and is
    // a no-op otherwise. Run this test filtered on its own (see the crate
    // docs) to guarantee it actually exercises the registry rather than
    // no-opping behind an earlier test's differently-keyed init.
    // ========================================================================

    /// Builds a temp directory containing a `master_skeleton.json` plus six
    /// minimal `.obj` parts — two independent head/torso combinations
    /// (`head1`/`torso1`, `head2`/`torso2`) and two distinct clothing
    /// items shared across both bodies — at fixed ids reserved for
    /// `concurrent_generation_and_prewarm_stress`, disjoint from every
    /// other fixed-id range used elsewhere in this file.
    fn make_temp_asset_dir_for_stress_test() -> (PathBuf, u32, u32, u32, u32, u32, u32) {
        const HEAD_1_ID: u32 = 30001;
        const TORSO_1_ID: u32 = 30002;
        const HEAD_2_ID: u32 = 30003;
        const TORSO_2_ID: u32 = 30004;
        const CLOTHING_A_ID: u32 = 30005;
        const CLOTHING_B_ID: u32 = 30006;

        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "anthroforge_test_assets_stress_{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("failed to create temp asset dir");

        std::fs::write(dir.join("master_skeleton.json"), r#"{"root": 0}"#)
            .expect("failed to write master_skeleton.json");

        // Two distinct triangles, purely so combination 1's parts aren't
        // byte-for-byte identical to combination 2's — not required for
        // correctness, just keeps the two bodies visibly distinguishable
        // if a failure ever needs debugging.
        let triangle_1 = "v 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 0.0 1.0 0.0\nf 1 2 3\n";
        let triangle_2 = "v 0.0 0.0 0.0\nv 2.0 0.0 0.0\nv 0.0 2.0 0.0\nf 1 2 3\n";

        std::fs::write(dir.join(format!("{HEAD_1_ID}_head1.obj")), triangle_1)
            .expect("failed to write head1 part");
        std::fs::write(dir.join(format!("{TORSO_1_ID}_torso1.obj")), triangle_1)
            .expect("failed to write torso1 part");
        std::fs::write(dir.join(format!("{HEAD_2_ID}_head2.obj")), triangle_2)
            .expect("failed to write head2 part");
        std::fs::write(dir.join(format!("{TORSO_2_ID}_torso2.obj")), triangle_2)
            .expect("failed to write torso2 part");
        std::fs::write(dir.join(format!("{CLOTHING_A_ID}_clothing_a.obj")), triangle_1)
            .expect("failed to write clothing part a");
        std::fs::write(dir.join(format!("{CLOTHING_B_ID}_clothing_b.obj")), triangle_2)
            .expect("failed to write clothing part b");

        (
            dir, HEAD_1_ID, TORSO_1_ID, HEAD_2_ID, TORSO_2_ID, CLOTHING_A_ID, CLOTHING_B_ID,
        )
    }

    /// Same idempotent-init convention as `registry_with_test_clothing_parts`:
    /// attempts its own init, then only returns `Some` if all six fixed ids
    /// from `make_temp_asset_dir_for_stress_test` ended up loaded into
    /// whichever registry state actually won the process-wide
    /// `GLOBAL_REGISTRY` race. Returns `None` otherwise, since the stress
    /// test below needs this specific two-body/two-clothing-item structure
    /// to run meaningfully.
    fn registry_with_stress_test_parts(
    ) -> Option<(&'static Registry, u32, u32, u32, u32, u32, u32)> {
        let (dir, head1, torso1, head2, torso2, clothing_a, clothing_b) =
            make_temp_asset_dir_for_stress_test();
        if GLOBAL_REGISTRY.get().is_none() {
            let dir_cstring = CString::new(dir.to_str().unwrap()).unwrap();
            let _ = init_part_registry(dir_cstring.as_ptr());
        }
        let registry = GLOBAL_REGISTRY.get()?;
        if registry.parts.contains_key(&head1)
            && registry.parts.contains_key(&torso1)
            && registry.parts.contains_key(&head2)
            && registry.parts.contains_key(&torso2)
            && registry.parts.contains_key(&clothing_a)
            && registry.parts.contains_key(&clothing_b)
        {
            Some((registry, head1, torso1, head2, torso2, clothing_a, clothing_b))
        } else {
            None
        }
    }

    /// Number of OS threads driving `generate_character` concurrently.
    /// 16 is a reasonable target for real contention on commodity CI
    /// hardware without an excessive test runtime.
    const STRESS_TEST_GENERATION_THREAD_COUNT: usize = 16;
    /// Iterations per generation thread. 100 is a reasonable target for
    /// the same reason as the thread count above — enough repeated load
    /// to give OS scheduling a real chance to interleave differently run
    /// to run, without the test taking too long.
    const STRESS_TEST_ITERATIONS_PER_THREAD: usize = 100;
    /// A small number of threads calling `prewarm_clothing_anchors_impl`
    /// directly, concurrently with the generation threads above.
    const STRESS_TEST_PREWARM_THREAD_COUNT: usize = 3;

    /// Real-OS-thread stress test for the two `RwLock`-guarded caches
    /// (`skin_tree_cache`, `clothing_anchor_cache`) plus `generate_character`
    /// and `prewarm_clothing_anchors_impl` running concurrently against
    /// them, simulating a game spawning many NPCs from multiple worker
    /// threads while a loading screen also prewarms clothing anchors.
    ///
    /// SCOPE NOTE — read before trusting a pass here too much: this test
    /// exercises real OS thread scheduling under repeated load, which is
    /// a genuine value-add (it catches a meaningful class of logic races
    /// and deadlocks on its own), but it is fundamentally probabilistic —
    /// a real data race might not manifest on every run, or on every
    /// machine. A pass here is evidence, not proof: it does **not** mean
    /// this code has been proven race-free the way a real Miri/ASan/TSan
    /// run would. That instrumented run happens separately, in this
    /// project's CI, on infrastructure where a real nightly toolchain is
    /// reachable — this test complements it, it does not replace it.
    #[test]
    fn concurrent_generation_and_prewarm_stress() {
        let Some((registry, head1, torso1, head2, torso2, clothing_a, clothing_b)) =
            registry_with_stress_test_parts()
        else {
            return;
        };

        let bodies = [(head1, torso1), (head2, torso2)];
        let clothing_items = [clothing_a, clothing_b];

        let mut handles: Vec<(String, std::thread::JoinHandle<()>)> = Vec::new();

        // Generation threads: each repeatedly calls `generate_character`,
        // varying which (head, torso) combination and which equipped
        // clothing item it uses across both threads and iterations, so
        // different threads race on genuinely shared cache keys (the same
        // combination hit by more than one thread) as well as genuinely
        // new ones (a combination being populated for the first time).
        for thread_index in 0..STRESS_TEST_GENERATION_THREAD_COUNT {
            let handle = std::thread::spawn(move || {
                for iteration in 0..STRESS_TEST_ITERATIONS_PER_THREAD {
                    let (head_id, torso_id) = bodies[(thread_index + iteration) % bodies.len()];
                    let clothing_id =
                        clothing_items[(thread_index + iteration) % clothing_items.len()];
                    let equipped_ids = [clothing_id];

                    let dna = CharacterDNA {
                        seed: (thread_index * STRESS_TEST_ITERATIONS_PER_THREAD + iteration)
                            as u64,
                        height_modifier: 1.0,
                        weight_modifier: 1.0,
                        head_id,
                        torso_id,
                        equipped_clothing_ids_ptr: equipped_ids.as_ptr(),
                        equipped_clothing_count: equipped_ids.len() as u32,
                    };

                    let buffer = generate_character(&dna as *const CharacterDNA);
                    if buffer.is_null() {
                        let message = last_error_message();
                        assert!(
                            message.as_deref().is_some_and(|m| !m.is_empty()),
                            "generation thread {thread_index}, iteration {iteration}: \
                             generate_character returned null with no specific \
                             last-error message"
                        );
                    } else {
                        free_mesh_buffer(buffer);
                    }
                }
            });
            handles.push((format!("generation thread {thread_index}"), handle));
        }

        // Prewarm threads: call the internal, synchronous
        // `prewarm_clothing_anchors_impl` directly (not the extern "C"
        // thread-spawning wrapper), concurrently with the generation
        // threads above, for the same (head, torso, clothing) combinations
        // those threads are using — this is specifically exercising the
        // interleaving between a real generation request lazily populating
        // the cache and a prewarm request proactively populating the same
        // entry, which no other test in this file does.
        for prewarm_index in 0..STRESS_TEST_PREWARM_THREAD_COUNT {
            let handle = std::thread::spawn(move || {
                for iteration in 0..STRESS_TEST_ITERATIONS_PER_THREAD {
                    let (head_id, torso_id) = bodies[(prewarm_index + iteration) % bodies.len()];
                    prewarm_clothing_anchors_impl(registry, head_id, torso_id, &clothing_items);
                }
            });
            handles.push((format!("prewarm thread {prewarm_index}"), handle));
        }

        // Join every spawned thread and explicitly check each `Result` —
        // under the default test harness, a panicking spawned thread does
        // NOT automatically fail the overall test unless its `join()`
        // result is checked.
        let mut panicked_threads: Vec<String> = Vec::new();
        for (name, handle) in handles {
            if handle.join().is_err() {
                panicked_threads.push(name);
            }
        }
        assert!(
            panicked_threads.is_empty(),
            "the following threads panicked during the concurrency stress test: {panicked_threads:?}"
        );

        // Final sanity check, single-threaded, after all concurrent load
        // has completed: every combination used above must still resolve
        // cleanly and return non-empty anchors, confirming the cache
        // didn't end up corrupted or stuck in a bad state.
        for &(head_id, torso_id) in &bodies {
            for &clothing_id in &clothing_items {
                let anchors = registry
                    .get_or_build_clothing_anchors(head_id, torso_id, clothing_id)
                    .unwrap_or_else(|e| {
                        panic!(
                            "post-stress sanity check failed for (head_id {head_id}, \
                             torso_id {torso_id}, clothing_id {clothing_id}): {e}"
                        )
                    });
                assert!(
                    !anchors.is_empty(),
                    "post-stress sanity check: anchors for (head_id {head_id}, \
                     torso_id {torso_id}, clothing_id {clothing_id}) were unexpectedly empty"
                );
            }
        }
    }

    // ========================================================================
    // Tests — `init_part_registry_from_pack` / the "AnthroForge Part Pack"
    // byte-format parser.
    // ========================================================================

    /// Builds a minimal, otherwise-well-formed single-triangle `.glb` in
    /// memory (3 vertices, one skinned triangle primitive, one named joint
    /// `"root"`) — the same construction `gltf_loader`'s own tests use,
    /// reproduced here since `gltf_loader::tests::build_test_glb` is
    /// private to that module.
    fn build_minimal_test_glb() -> Vec<u8> {
        let positions: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals: [[f32; 3]; 3] = [[0.0, 0.0, 1.0]; 3];
        let texcoords: [[f32; 2]; 3] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        let joints: [[u8; 4]; 3] = [[0, 0, 0, 0]; 3];
        let weights: [[f32; 4]; 3] = [[1.0, 0.0, 0.0, 0.0]; 3];
        let indices: [u16; 3] = [0, 1, 2];

        let mut bin: Vec<u8> = Vec::new();
        for p in &positions {
            for c in p {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let normals_offset = bin.len();
        for n in &normals {
            for c in n {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let texcoords_offset = bin.len();
        for t in &texcoords {
            for c in t {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let joints_offset = bin.len();
        for j in &joints {
            bin.extend_from_slice(j);
        }
        let weights_offset = bin.len();
        for w in &weights {
            for c in w {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let indices_offset = bin.len();
        for &i in &indices {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        let indices_byte_len = indices.len() * 2;
        let buffer_byte_len = bin.len();

        while bin.len() % 4 != 0 {
            bin.push(0);
        }

        let json = serde_json::json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [
                { "mesh": 0, "skin": 0 },
                { "name": "root" }
            ],
            "meshes": [{
                "primitives": [{
                    "attributes": {
                        "POSITION": 0,
                        "NORMAL": 1,
                        "TEXCOORD_0": 2,
                        "JOINTS_0": 3,
                        "WEIGHTS_0": 4
                    },
                    "indices": 5,
                    "mode": 4
                }]
            }],
            "skins": [{ "joints": [1] }],
            "accessors": [
                { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0] },
                { "bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3" },
                { "bufferView": 2, "componentType": 5126, "count": 3, "type": "VEC2" },
                { "bufferView": 3, "componentType": 5121, "count": 3, "type": "VEC4" },
                { "bufferView": 4, "componentType": 5126, "count": 3, "type": "VEC4" },
                { "bufferView": 5, "componentType": 5123, "count": indices.len(), "type": "SCALAR" }
            ],
            "bufferViews": [
                { "buffer": 0, "byteOffset": 0, "byteLength": normals_offset },
                { "buffer": 0, "byteOffset": normals_offset, "byteLength": texcoords_offset - normals_offset },
                { "buffer": 0, "byteOffset": texcoords_offset, "byteLength": joints_offset - texcoords_offset },
                { "buffer": 0, "byteOffset": joints_offset, "byteLength": weights_offset - joints_offset },
                { "buffer": 0, "byteOffset": weights_offset, "byteLength": indices_offset - weights_offset },
                { "buffer": 0, "byteOffset": indices_offset, "byteLength": indices_byte_len }
            ],
            "buffers": [{ "byteLength": buffer_byte_len }]
        });
        let mut json_bytes = serde_json::to_vec(&json).expect("json serializes");
        while json_bytes.len() % 4 != 0 {
            json_bytes.push(b' ');
        }

        let total_len = 12 + 8 + json_bytes.len() + 8 + bin.len();
        let mut glb = Vec::with_capacity(total_len);
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total_len as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    /// Hand-builds a minimal, valid Part Pack buffer, byte-by-byte, per the
    /// frozen format: one `src_type = 0` (OBJ) part and one `src_type = 1`
    /// (glTF/GLB) part, against a `master_skeleton.json` naming the one
    /// bone both parts reference (`"root"` — see `build_minimal_test_glb`'s
    /// doc comment for why both parts share that one name rather than the
    /// glTF part using a distinct joint name). Deliberately lays the two
    /// parts' data out in the buffer in the *opposite* order from their
    /// index entries, to exercise the format's "parts may appear in a
    /// different order than their index entries; always seek by
    /// data_offset/data_len" rule. Returns the pack bytes plus the two
    /// chosen part ids.
    fn build_minimal_part_pack() -> (Vec<u8>, u32, u32) {
        // Both parts' one bone is named "root" — deliberate: the OBJ part
        // rigidly binds to `obj_loader::OBJ_BIND_BONE_NAME` ("root"), which
        // never itself carries bind-pose/hierarchy data (a plain OBJ mesh
        // has none to give); the glTF part's single joint is *also* named
        // "root" so it's the one that actually contributes that bone's
        // entry to the assembled global skeleton (see
        // `skeleton_resolver::contribute_global_joints`), satisfying
        // `init_part_registry*`'s completeness check. A `master_skeleton.json`
        // naming a bone that only OBJ-bound parts ever reference (and that
        // no glTF part ever contributes real transform data for) is
        // correctly rejected by that check — see the Phase 4
        // skeleton-extraction task write-up's handoff notes.
        let skeleton_json = br#"{"root":0}"#.to_vec();
        let obj_bytes = b"v 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 0.0 1.0 0.0\nf 1 2 3\n".to_vec();
        let glb_bytes = build_minimal_test_glb();

        let obj_part_id: u32 = 90001;
        let gltf_part_id: u32 = 90002;

        const HEADER_LEN: usize = 16;
        const INDEX_ENTRY_LEN: usize = 13;
        let index_len = 2 * INDEX_ENTRY_LEN;

        let data_region_start = HEADER_LEN + skeleton_json.len() + index_len;
        // glTF part's bytes come first in the data region; OBJ part's index
        // entry comes first in the index table. Order deliberately mismatched.
        let glb_offset = data_region_start;
        let obj_offset = glb_offset + glb_bytes.len();

        let mut buf = Vec::new();
        buf.extend_from_slice(b"AFPP");
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&(skeleton_json.len() as u32).to_le_bytes());
        buf.extend_from_slice(&skeleton_json);

        // Index entry 0: the OBJ part.
        buf.extend_from_slice(&obj_part_id.to_le_bytes());
        buf.push(0u8);
        buf.extend_from_slice(&(obj_offset as u32).to_le_bytes());
        buf.extend_from_slice(&(obj_bytes.len() as u32).to_le_bytes());

        // Index entry 1: the glTF part.
        buf.extend_from_slice(&gltf_part_id.to_le_bytes());
        buf.push(1u8);
        buf.extend_from_slice(&(glb_offset as u32).to_le_bytes());
        buf.extend_from_slice(&(glb_bytes.len() as u32).to_le_bytes());

        // Data region: glTF bytes first, then OBJ bytes (opposite of index order).
        buf.extend_from_slice(&glb_bytes);
        buf.extend_from_slice(&obj_bytes);

        (buf, obj_part_id, gltf_part_id)
    }

    /// End-to-end: a hand-built, valid 2-part Part Pack loads successfully
    /// via `init_part_registry_from_pack`, and the resulting registry
    /// contains exactly those 2 parts. `GLOBAL_REGISTRY` is process-wide
    /// (can only be initialized once per test binary run), so — matching
    /// this file's existing convention for `GLOBAL_REGISTRY`-dependent
    /// tests — if some other test already won the init race first, this
    /// test is a no-op rather than a false failure.
    #[test]
    fn init_part_registry_from_pack_loads_hand_built_pack() {
        if GLOBAL_REGISTRY.get().is_some() {
            return;
        }

        let (pack_bytes, obj_part_id, gltf_part_id) = build_minimal_part_pack();
        let ok = init_part_registry_from_pack(pack_bytes.as_ptr(), pack_bytes.len());

        if !ok {
            // Another test may have won the registry-init race between our
            // check above and this call; that's the one legitimate reason
            // this could fail without indicating a real bug.
            if GLOBAL_REGISTRY.get().is_some() {
                return;
            }
            let message = last_error_message().unwrap_or_default();
            panic!("well-formed hand-built pack must load successfully, got error: {message}");
        }

        let registry = GLOBAL_REGISTRY
            .get()
            .expect("registry must be set after a successful init_part_registry_from_pack call");
        assert_eq!(registry.parts.len(), 2, "expected exactly 2 loaded parts");
        assert!(
            registry.parts.contains_key(&obj_part_id),
            "expected the OBJ part id {obj_part_id} to be loaded"
        );
        assert!(
            registry.parts.contains_key(&gltf_part_id),
            "expected the glTF part id {gltf_part_id} to be loaded"
        );
    }

    /// Negative test: a Part Pack buffer with bad magic bytes must be
    /// rejected with `false` and a specific, non-generic last-error
    /// message — never a panic — following this file's existing convention
    /// for asserting on `anthroforge_last_error()` after a failing call
    /// (see `init_part_registry_null_asset_dir_sets_last_error` above).
    ///
    /// `GLOBAL_REGISTRY` is process-wide and can only be initialized once
    /// per test binary run: `init_part_registry_from_pack_impl` checks
    /// "already initialized" *before* it ever looks at the magic bytes
    /// (per the frozen check ordering: null pointer, then already-
    /// initialized, then header parsing), so if some other test already
    /// won that race first, this call legitimately fails with "registry
    /// already initialized" instead of a magic-bytes message. Both are
    /// specific, non-generic errors, so — mirroring
    /// `generate_character_before_registry_init_sets_specific_last_error`'s
    /// existing two-branch pattern above — this test asserts whichever one
    /// applies rather than assuming a fresh registry.
    #[test]
    fn init_part_registry_from_pack_bad_magic_sets_last_error() {
        // Only the magic bytes are wrong; the rest of the header is
        // well-formed-looking (version 1, part_count 0, skel_len 0) so this
        // specifically exercises the magic check rather than some other
        // early bailout (length, null pointer, etc) when it is reached.
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(b"XXXX");
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());

        let registry_was_already_initialized = GLOBAL_REGISTRY.get().is_some();

        let ok = init_part_registry_from_pack(buf.as_ptr(), buf.len());
        assert!(!ok, "bad magic bytes must be rejected");

        let message =
            last_error_message().expect("a bad-magic failure must set a specific last error");
        if registry_was_already_initialized {
            assert!(
                message.contains("registry already initialized"),
                "expected a message about the registry already being initialized, got: {message}"
            );
        } else {
            assert!(
                message.contains("bad magic bytes"),
                "expected a message about bad magic bytes, got: {message}"
            );
        }
    }
}
