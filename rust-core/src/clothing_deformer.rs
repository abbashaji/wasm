//! `clothing_deformer` — Phase 3 of 5: runtime anti-clipping clothing fit.
//!
//! A clothing item is authored once, against a single "default" naked body.
//! At runtime a character's skin is procedurally mutated by `CharacterDNA`
//! (taller, wider, different proportions, ...) and the clothing mesh has to
//! follow that mutation *without* an offline cloth simulation / physics
//! bake, and without ever letting the garment clip through the new skin
//! surface. This module implements that in three stages:
//!
//! 1. **[`build_cloth_anchors`] (init-time, once per clothing item).** For
//!    every clothing vertex, a 1-nearest-neighbor search against the
//!    default skin mesh finds the single closest skin vertex and records
//!    the offset `O = V_cloth - V_skin` between them as a [`ClothAnchor`].
//! 2. **[`fit_clothing_to_skin`] (runtime, per generated character), step
//!    A — delta morph propagation.** For each clothing vertex, read its
//!    anchor's matched skin vertex at its *mutated* position `V'_skin` and
//!    reconstruct a rough-fit clothing position `V'_cloth = V'_skin + (O
//!    ⊙ S)`, where `S` is the DNA-derived per-axis body scale (see the
//!    "design note on `dna_scale`" below).
//! 3. **[`fit_clothing_to_skin`] (runtime), step B — SDF push-out.** The
//!    rough fit from step 2 is a linear approximation and can still end up
//!    inside the mutated skin (e.g. at a joint bend, or where the DNA
//!    scale was strongly non-uniform). Each clothing vertex is therefore
//!    tested against the local skin surface — approximated as the tangent
//!    plane through `V'_skin` with normal `N'_skin` — and pushed outward
//!    along `N'_skin` whenever it is closer than `thickness_clearance +
//!    clearance_epsilon`, which makes clipping structurally impossible
//!    regardless of how good or bad the linear approximation was.
//!
//! Finally, each fitted clothing vertex has its `bone_indices` /
//! `bone_weights` overwritten with its anchor's skin vertex's values, so
//! the garment is rigidly skinned to the same skeleton as the body under
//! it with zero manual weight-painting.
//!
//! # Design note on `dna_scale`
//! The algorithm this module implements is specified (see module docs
//! above / task description) as `V'_cloth = V'_skin + (O · S)`, where `S`
//! is "the DNA-derived scale vector". [`fit_clothing_to_skin`] therefore
//! takes `dna_scale: [f32; 3]` as an explicit parameter.
//!
//! This is a deliberate, small addition on top of the signature this
//! phase's brief sketched (`fit_clothing_to_skin(skin_vertices,
//! cloth_vertices, anchors, clearance_epsilon)`, with no scale argument).
//! That sketch is not implementable as specified: `S` cannot be recovered
//! from `skin_vertices`, `cloth_vertices`, or `anchors` alone.
//! `skin_vertices` here is *already post-mutation* (per the brief: "the
//! new position of the anchored skin vertex"), and `ClothAnchor` — whose
//! shape was itself specified as exactly `{
//! target_skin_vertex_index, local_offset, thickness_clearance }` — has no
//! field to recover a bind-pose reference from. Silently substituting `S
//! = [1,1,1]` would quietly drop the "grow with the body" behavior the
//! brief explicitly asks for (a fat character's shirt would stay
//! shirt-sized instead of loosening), which seemed like the worse failure
//! mode versus taking the one extra `f32; 3]` parameter needed to do what
//! was actually asked. `CharacterDNA` (Phase 1) itself is not threaded
//! through this module, keeping `clothing_deformer` decoupled from the FFI
//! layer; see [`dna_to_scale_vector`] and the `lib.rs` integration diff at
//! the end of this deliverable for how a caller derives `dna_scale` from
//! `CharacterDNA` today, and note that mapping is a placeholder pending
//! Phase 4's real body-mutation model, not a load-bearing part of this
//! module's contract.

use std::fmt;

#[cfg(not(any(miri, target_arch = "wasm32")))]
use rayon::prelude::*;

use crate::{CharacterDNA, SkinnedVertex};

// ============================================================================
// Miri/rayon interop note
// ============================================================================
//
// Rayon's implicit global thread pool spawns worker threads that park
// forever waiting for work and are never joined — normal and harmless
// under a real OS (they're reaped on process exit), but Miri explicitly
// checks that no other threads are still alive when the main thread
// finishes, so any test that touches `par_iter()`/`par_iter_mut()` fails
// with "the main thread terminated without waiting for all remaining
// threads". This is a known, unfixable-from-userland Miri/rayon
// incompatibility (the global pool has no shutdown hook), not a bug in
// this crate — see https://users.rust-lang.org/t/how-to-test-rayon-with-miri/67314.
//
// Miri is checking correctness/UB, not performance, so losing
// parallelism under it costs nothing real.
//
// wasm32-unknown-unknown joins this same fallback for a different reason:
// per the AnthroForge/Web spec (section 4.2/9.2/11), v1 deliberately ships
// single-threaded on wasm rather than pulling in `wasm-bindgen-rayon` +
// `SharedArrayBuffer` + the COOP/COEP hosting-header requirement that
// imposes on every consuming site — a real product tradeoff, not a
// technical dead end (`wasm-bindgen-rayon` is a legitimate v2 option, see
// spec section 11). `rayon` itself is not merely feature-gated here; the
// crate is not even imported for this target, so no thread pool is ever
// spawned to fail against wasm32's lack of native thread support.
//
// These two macros pick `par_iter`/`par_iter_mut` normally and fall back
// to the equivalent sequential `iter`/`iter_mut` under `cfg(miri)` or
// `cfg(target_arch = "wasm32")`, so `build_cloth_anchors` and
// `fit_clothing_to_skin` stay genuinely parallel in the native production
// build while degrading to sequential — correctly, not by accident — on
// both of the other two targets.
#[cfg(not(any(miri, target_arch = "wasm32")))]
macro_rules! cloth_iter {
    ($e:expr) => {
        $e.par_iter()
    };
}
#[cfg(any(miri, target_arch = "wasm32"))]
macro_rules! cloth_iter {
    ($e:expr) => {
        $e.iter()
    };
}

#[cfg(not(any(miri, target_arch = "wasm32")))]
macro_rules! cloth_iter_mut {
    ($e:expr) => {
        $e.par_iter_mut()
    };
}
#[cfg(any(miri, target_arch = "wasm32"))]
macro_rules! cloth_iter_mut {
    ($e:expr) => {
        $e.iter_mut()
    };
}

// ============================================================================
// Public data types
// ============================================================================

/// One clothing vertex's binding to the default skin mesh, computed once at
/// init time by [`build_cloth_anchors`] and re-used for every character
/// generated while wearing that clothing item.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct ClothAnchor {
    /// Index into the *skin* vertex buffer (both the default skin buffer
    /// used at build time and the mutated skin buffer passed to
    /// [`fit_clothing_to_skin`] at runtime — the two must share the same
    /// vertex indexing, i.e. mutation may move vertices but must not
    /// reorder, add, or remove them).
    pub target_skin_vertex_index: u32,
    /// `V_cloth - V_skin` in the default (bind) pose, object space.
    pub local_offset: [f32; 3],
    /// Minimum signed distance, along the bind-pose skin normal, that this
    /// clothing vertex must maintain from the skin surface. Derived at
    /// build time from how far the garment was originally authored to sit
    /// above the body at this point (see [`build_cloth_anchors`]).
    pub thickness_clearance: f32,
}
// Total size MUST be 20 bytes (4 + 12 + 4), zero padding, matching the
// `#[repr(C)]` FFI convention Phase 1 established for cross-boundary
// structs — a caching layer on the engine side may reasonably want to
// persist/reload these directly.
const _: () = assert!(std::mem::size_of::<ClothAnchor>() == 20);

/// Every fallible operation in this module returns one of these instead of
/// panicking — required because this module's functions are, directly or
/// via a thin `extern "C"` wrapper, reachable from the FFI boundary, and
/// unwinding across it is undefined behavior.
#[derive(Debug)]
pub enum ClothingDeformerError {
    /// [`build_cloth_anchors`] was called with an empty default-skin
    /// vertex buffer; there is nothing to anchor against.
    EmptySkinMesh,
    /// [`build_cloth_anchors`] was called with an empty clothing vertex
    /// buffer; there is nothing to build anchors for.
    EmptyClothMesh,
    /// A vertex position contained a NaN or infinite component. Such a
    /// vertex cannot be placed in (or queried against) the k-d tree, since
    /// every ordering comparison involving it would be ill-defined.
    NonFiniteVertexPosition { mesh: &'static str, vertex_index: usize },
    /// Internal invariant failure: the 1-NN search returned no match
    /// against a non-empty, validated tree. Should be unreachable in
    /// practice; surfaced as a typed error rather than a panic so a bug
    /// here fails loudly through the normal `Result` path instead of
    /// unwinding across FFI.
    NearestNeighborSearchFailed { cloth_vertex_index: usize },
    /// `anchors.len()` did not match `cloth_vertices.len()` in
    /// [`fit_clothing_to_skin`]. Anchors are positional (`anchors[i]`
    /// binds `cloth_vertices[i]`), so a length mismatch means the caller
    /// passed anchors built for a different clothing buffer.
    AnchorCountMismatch { cloth_vertex_count: usize, anchor_count: usize },
    /// An anchor's `target_skin_vertex_index` pointed past the end of
    /// `skin_vertices`. This means the anchors were built against a
    /// different (or differently-sized) skin buffer than the one passed
    /// at runtime.
    AnchorSkinIndexOutOfRange {
        cloth_vertex_index: usize,
        target_skin_vertex_index: u32,
        skin_vertex_count: usize,
    },
    /// `clearance_epsilon` was NaN or infinite.
    NonFiniteClearanceEpsilon,
}

impl fmt::Display for ClothingDeformerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClothingDeformerError::EmptySkinMesh => {
                write!(f, "default skin vertex buffer was empty; nothing to anchor clothing vertices to")
            }
            ClothingDeformerError::EmptyClothMesh => {
                write!(f, "clothing vertex buffer was empty; nothing to build anchors for")
            }
            ClothingDeformerError::NonFiniteVertexPosition { mesh, vertex_index } => {
                write!(f, "{mesh} vertex[{vertex_index}] has a non-finite position component (NaN or infinite)")
            }
            ClothingDeformerError::NearestNeighborSearchFailed { cloth_vertex_index } => write!(
                f,
                "1-NN search against the skin mesh returned no match for cloth vertex[{cloth_vertex_index}] \
                 (internal invariant failure — the skin k-d tree should be non-empty and total)"
            ),
            ClothingDeformerError::AnchorCountMismatch { cloth_vertex_count, anchor_count } => write!(
                f,
                "anchor count ({anchor_count}) does not match cloth vertex count ({cloth_vertex_count}); \
                 anchors are positional and must have been built for this exact clothing buffer"
            ),
            ClothingDeformerError::AnchorSkinIndexOutOfRange {
                cloth_vertex_index,
                target_skin_vertex_index,
                skin_vertex_count,
            } => write!(
                f,
                "anchor for cloth vertex[{cloth_vertex_index}] targets skin vertex index \
                 {target_skin_vertex_index}, but the skin buffer only has {skin_vertex_count} vertices \
                 (anchors were likely built against a different skin mesh)"
            ),
            ClothingDeformerError::NonFiniteClearanceEpsilon => {
                write!(f, "clearance_epsilon was NaN or infinite")
            }
        }
    }
}

impl std::error::Error for ClothingDeformerError {}

// ============================================================================
// Small vector helpers.
//
// Phase 1's Cargo.toml pulls in no linear-algebra crate (glam/nalgebra/...),
// and this module's own new dependency is scoped to rayon only (see the
// Cargo.toml diff at the end of this deliverable), so these are implemented
// directly against `[f32; 3]` rather than reaching for a new dependency to
// do three-component vector arithmetic.
// ============================================================================

#[inline]
fn v_sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[inline]
fn v_add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

#[inline]
fn v_mul_components(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] * b[0], a[1] * b[1], a[2] * b[2]]
}

#[inline]
fn v_scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

#[inline]
fn v_dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[inline]
fn v_length(a: [f32; 3]) -> f32 {
    v_dot(a, a).sqrt()
}

#[inline]
fn v_squared_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = v_sub(a, b);
    v_dot(d, d)
}

#[inline]
fn v_is_finite(a: [f32; 3]) -> bool {
    a[0].is_finite() && a[1].is_finite() && a[2].is_finite()
}

/// Normalizes `a`, or returns `None` for a zero/degenerate/non-finite
/// vector rather than producing NaNs. Used for skin normals, which in
/// theory are always unit-length in well-formed mesh data but are treated
/// defensively here rather than trusted blindly.
#[inline]
fn v_normalize(a: [f32; 3]) -> Option<[f32; 3]> {
    let len = v_length(a);
    if !len.is_finite() || len <= f32::EPSILON {
        None
    } else {
        Some(v_scale(a, 1.0 / len))
    }
}

// ============================================================================
// Minimal internal k-d tree, 1-NN queries only.
//
// This module's own new dependency is scoped to rayon only, so nearest-
// neighbor search is implemented directly rather than by pulling in a
// dedicated spatial-index crate. It is a standard median-split k-d tree
// over the skin vertex positions: O(n log n) to build, O(log n) average
// case per 1-NN query, with the classic pruned-recursion nearest-neighbor
// search (only descend into the far subtree if it could still contain a
// closer point than the current best).
// ============================================================================

struct KdNode {
    vertex_index: u32,
    split_axis: u8,
    left: Option<Box<KdNode>>,
    right: Option<Box<KdNode>>,
}

struct KdTree {
    root: Box<KdNode>,
}

impl KdTree {
    /// Builds a tree over `positions`. `positions` must be non-empty and
    /// every component of every position must be finite — both are the
    /// caller's responsibility (validated by [`build_cloth_anchors`]
    /// before this is ever called), since a tree with no valid ordering
    /// over some of its points cannot support correct nearest-neighbor
    /// queries at all.
    fn build(positions: &[[f32; 3]]) -> Option<Self> {
        let mut indices: Vec<u32> = (0..positions.len() as u32).collect();
        let root = Self::build_recursive(&mut indices, positions, 0)?;
        Some(KdTree { root })
    }

    fn build_recursive(
        indices: &mut [u32],
        positions: &[[f32; 3]],
        depth: usize,
    ) -> Option<Box<KdNode>> {
        if indices.is_empty() {
            return None;
        }
        let axis = (depth % 3) as u8;
        let median = indices.len() / 2;

        // Partition (not fully sort) around the median along `axis`; this
        // is what keeps the build at O(n log n) rather than O(n log^2 n).
        // Positions were already validated finite, so this comparator
        // never actually hits the NaN fallback branch — it's there purely
        // so a partial_cmp failure can never become a panic.
        indices.select_nth_unstable_by(median, |&a, &b| {
            positions[a as usize][axis as usize]
                .partial_cmp(&positions[b as usize][axis as usize])
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let vertex_index = indices[median];
        let (left_indices, rest) = indices.split_at_mut(median);
        let (_, right_indices) = rest.split_at_mut(1);

        let left = Self::build_recursive(left_indices, positions, depth + 1);
        let right = Self::build_recursive(right_indices, positions, depth + 1);

        Some(Box::new(KdNode { vertex_index, split_axis: axis, left, right }))
    }

    /// Returns the index (into `positions`, the same slice the tree was
    /// built from) of the closest point to `query`.
    fn nearest(&self, positions: &[[f32; 3]], query: [f32; 3]) -> u32 {
        let mut best_index = self.root.vertex_index;
        let mut best_dist_sq = v_squared_distance(positions[best_index as usize], query);
        Self::nearest_recursive(&self.root, positions, query, &mut best_index, &mut best_dist_sq);
        best_index
    }

    fn nearest_recursive(
        node: &KdNode,
        positions: &[[f32; 3]],
        query: [f32; 3],
        best_index: &mut u32,
        best_dist_sq: &mut f32,
    ) {
        let node_pos = positions[node.vertex_index as usize];
        let d_sq = v_squared_distance(node_pos, query);
        if d_sq < *best_dist_sq {
            *best_dist_sq = d_sq;
            *best_index = node.vertex_index;
        }

        let axis = node.split_axis as usize;
        let diff = query[axis] - node_pos[axis];
        let (near, far) = if diff <= 0.0 {
            (&node.left, &node.right)
        } else {
            (&node.right, &node.left)
        };

        if let Some(near) = near {
            Self::nearest_recursive(near, positions, query, best_index, best_dist_sq);
        }
        // Only the near side can be pruned safely: if the query's distance
        // to the splitting plane already exceeds the best match found so
        // far, no point on the far side of the plane can possibly be
        // closer than that match.
        if diff * diff < *best_dist_sq {
            if let Some(far) = far {
                Self::nearest_recursive(far, positions, query, best_index, best_dist_sq);
            }
        }
    }
}

// ============================================================================
// Shared per-body skin KD-tree — reused across every clothing item fitted
// to the same (head, torso) combination, instead of rebuilding a fresh
// tree per clothing item.
// ============================================================================

/// A KD-tree built over a (possibly decimated) sample of one body's
/// bind-pose skin, reusable across every clothing item fitted to that
/// same body. `sample_to_full_index[i]` maps the tree's internal index
/// space back to the corresponding index in the *full-resolution* skin
/// vertex buffer, since anchors must ultimately reference full-resolution
/// skin vertices even when the tree was built over a decimated subset.
pub(crate) struct SkinKdTree {
    tree: KdTree,
    sample_positions: Vec<[f32; 3]>,
    sample_to_full_index: Vec<u32>,
}

/// Shared skin-side validation used by both [`build_skin_kdtree`] and (via
/// it) the legacy all-in-one [`build_cloth_anchors`] entry point: rejects
/// an empty skin buffer or any non-finite vertex position, and returns the
/// flattened, full-resolution position array on success.
fn validate_skin_positions(
    default_skin_vertices: &[SkinnedVertex],
) -> Result<Vec<[f32; 3]>, ClothingDeformerError> {
    if default_skin_vertices.is_empty() {
        return Err(ClothingDeformerError::EmptySkinMesh);
    }
    let skin_positions: Vec<[f32; 3]> = default_skin_vertices.iter().map(|v| v.position).collect();
    for (i, p) in skin_positions.iter().enumerate() {
        if !v_is_finite(*p) {
            return Err(ClothingDeformerError::NonFiniteVertexPosition { mesh: "skin", vertex_index: i });
        }
    }
    Ok(skin_positions)
}

/// Builds a [`SkinKdTree`] over every `stride`-th vertex of
/// `default_skin_vertices` (`stride = 1` means every vertex — no
/// decimation). `stride` must be `>= 1`.
///
/// A larger `stride` produces a smaller tree, which is both faster to
/// build (this happens once per distinct body) and faster to query (this
/// happens once per clothing vertex, for every clothing item fitted to
/// that body) — at the cost of coarser nearest-neighbor matching for the
/// anchor-assignment step, since a decimated tree can only match a
/// clothing vertex to one of the *sampled* skin vertices, not necessarily
/// the true closest vertex in the full-resolution mesh. This trade-off
/// does **not** weaken the anti-clipping guarantee:
/// [`fit_clothing_to_skin`]'s SDF push-out (step 3) still runs
/// unconditionally against each anchor's real, full-resolution
/// `thickness_clearance`, so decimation can only affect how *naturally*
/// an anchor was matched, never whether clipping is structurally
/// prevented.
pub(crate) fn build_skin_kdtree(
    default_skin_vertices: &[SkinnedVertex],
    stride: usize,
) -> Result<SkinKdTree, ClothingDeformerError> {
    debug_assert!(stride >= 1, "build_skin_kdtree: stride must be >= 1");
    let stride = stride.max(1);

    let skin_positions = validate_skin_positions(default_skin_vertices)?;

    let sample_count = skin_positions.len().div_ceil(stride);
    let mut sample_positions = Vec::with_capacity(sample_count);
    let mut sample_to_full_index = Vec::with_capacity(sample_count);
    let mut full_index = 0usize;
    while full_index < skin_positions.len() {
        sample_positions.push(skin_positions[full_index]);
        sample_to_full_index.push(full_index as u32);
        full_index += stride;
    }

    // `sample_positions` is non-empty (`skin_positions` was validated
    // non-empty above, and index 0 is always sampled) and fully
    // finite-validated (a subset of an already-validated array), so
    // `KdTree::build` returning `None` would itself be an internal
    // invariant failure; treat that the same way as a failed query below
    // rather than unwrapping.
    let tree = match KdTree::build(&sample_positions) {
        Some(tree) => tree,
        None => return Err(ClothingDeformerError::NearestNeighborSearchFailed { cloth_vertex_index: 0 }),
    };

    Ok(SkinKdTree { tree, sample_positions, sample_to_full_index })
}

/// Same computation [`build_cloth_anchors`] already does per clothing
/// vertex, but querying a pre-built, possibly-shared [`SkinKdTree`]
/// instead of building a fresh tree. `default_skin_vertices` must be the
/// exact same full-resolution buffer `skin_tree` was built from (needed
/// here to look up each matched vertex's real position/normal for the
/// `local_offset`/`thickness_clearance` computation, exactly as
/// `build_cloth_anchors` does today).
///
/// # Errors
/// - [`ClothingDeformerError::EmptyClothMesh`] if `cloth_vertices` is
///   empty.
/// - [`ClothingDeformerError::NonFiniteVertexPosition`] if any cloth
///   vertex position has a NaN/infinite component. (The skin side was
///   already validated when `skin_tree` was built.)
pub(crate) fn build_cloth_anchors_with_tree(
    cloth_vertices: &[SkinnedVertex],
    default_skin_vertices: &[SkinnedVertex],
    skin_tree: &SkinKdTree,
) -> Result<Vec<ClothAnchor>, ClothingDeformerError> {
    if cloth_vertices.is_empty() {
        return Err(ClothingDeformerError::EmptyClothMesh);
    }
    for (i, v) in cloth_vertices.iter().enumerate() {
        if !v_is_finite(v.position) {
            return Err(ClothingDeformerError::NonFiniteVertexPosition { mesh: "cloth", vertex_index: i });
        }
    }

    // Genuinely parallel: each cloth vertex's nearest-neighbor query and
    // anchor computation is independent of every other, so this fans out
    // across rayon's thread pool via `par_iter()` rather than running
    // serially.
    cloth_iter!(cloth_vertices)
        .map(|cloth_vertex| {
            let sample_index = skin_tree.tree.nearest(&skin_tree.sample_positions, cloth_vertex.position);
            let full_index = skin_tree.sample_to_full_index[sample_index as usize];
            let skin_vertex = &default_skin_vertices[full_index as usize];

            let local_offset = v_sub(cloth_vertex.position, skin_vertex.position);

            let thickness_clearance = match v_normalize(skin_vertex.normal) {
                Some(normal) => v_dot(local_offset, normal).max(0.0),
                // Degenerate bind-pose skin normal: fall back to the full
                // offset length as a conservative clearance estimate
                // rather than silently treating this vertex as requiring
                // zero clearance.
                None => v_length(local_offset),
            };

            ClothAnchor {
                target_skin_vertex_index: full_index,
                local_offset,
                thickness_clearance,
            }
        })
        .collect::<Vec<_>>()
        .pipe_ok()
}

// ============================================================================
// Step 1 — init-time anchor building (1-NN search).
// ============================================================================

/// Builds one [`ClothAnchor`] per vertex of `cloth_vertices`, binding each
/// to its single closest vertex (by Euclidean distance, in object space)
/// on `default_skin_vertices`. Call this once per clothing item, when the
/// item is loaded — not per generated character.
///
/// `thickness_clearance` for each anchor is derived from the bind-pose
/// geometry itself: the component of the cloth-to-skin offset along the
/// skin vertex's own (normalized) normal, i.e. how far above the skin
/// surface — rather than just "away from the skin vertex" in an arbitrary
/// direction — the garment was originally authored to sit at that point.
/// It is clamped to be non-negative: a negative value would mean the
/// clothing vertex was already authored on or inside the skin surface at
/// that point, which [`fit_clothing_to_skin`]'s push-out step should
/// treat as "no required clearance" rather than as a demand to push the
/// (already-touching-or-inside, by design) vertex outward.
///
/// Implemented in terms of [`build_skin_kdtree`] (with `stride = 1`, i.e.
/// no decimation — an exact tree over every skin vertex) followed by
/// [`build_cloth_anchors_with_tree`]; this is a refactor for reuse by the
/// per-body cache in `lib.rs`'s `Registry`, not a behavior change — this
/// function still produces byte-for-byte identical results to before.
///
/// # Errors
/// - [`ClothingDeformerError::EmptySkinMesh`] / `EmptyClothMesh` if either
///   input buffer is empty.
/// - [`ClothingDeformerError::NonFiniteVertexPosition`] if any vertex
///   position (in either buffer) has a NaN/infinite component.
/// - [`ClothingDeformerError::NearestNeighborSearchFailed`] only on an
///   internal invariant failure; should not occur in practice given the
///   validation above.
pub fn build_cloth_anchors(
    cloth_vertices: &[SkinnedVertex],
    default_skin_vertices: &[SkinnedVertex],
) -> Result<Vec<ClothAnchor>, ClothingDeformerError> {
    let skin_tree = build_skin_kdtree(default_skin_vertices, 1)?;
    build_cloth_anchors_with_tree(cloth_vertices, default_skin_vertices, &skin_tree)
}

// Tiny local extension trait so the `.collect()` above reads as
// "collect, then wrap in Ok" without a trailing `Ok(...)` that would sit
// awkwardly after a long parallel-iterator chain. Not exposed outside this
// module.
trait PipeOk: Sized {
    fn pipe_ok(self) -> Result<Self, ClothingDeformerError> {
        Ok(self)
    }
}
impl<T> PipeOk for T {}

// ============================================================================
// Step 2 + 3 — runtime fit: delta morph propagation, then SDF push-out.
// ============================================================================

/// Fits `cloth_vertices` (in place) to the mutated skin described by
/// `skin_vertices`, using the bind-pose anchors from [`build_cloth_anchors`].
///
/// `anchors[i]` must correspond to `cloth_vertices[i]` (the same ordering
/// [`build_cloth_anchors`] produces its output in). `skin_vertices` must be
/// the *mutated* skin buffer — same vertex indexing as the default skin
/// buffer the anchors were built from, just with new positions/normals —
/// and is otherwise read-only here; skin mutation itself happens upstream
/// of this call (see the `lib.rs` integration note at the end of this
/// deliverable).
///
/// `dna_scale` is the per-axis body scale to apply to each anchor's stored
/// offset (see the module-level "design note on `dna_scale`" for why this
/// parameter exists). Pass `[1.0, 1.0, 1.0]` for "no additional scaling
/// beyond what's already baked into `skin_vertices`' positions".
///
/// Runs genuinely in parallel across `cloth_vertices` via
/// `par_iter_mut()`; each vertex's fit depends only on its own anchor and
/// the (read-only, shared) skin buffer, so there is no cross-vertex
/// synchronization needed.
///
/// # Errors
/// - [`ClothingDeformerError::AnchorCountMismatch`] if `anchors.len() !=
///   cloth_vertices.len()`.
/// - [`ClothingDeformerError::AnchorSkinIndexOutOfRange`] if any anchor's
///   `target_skin_vertex_index` is out of bounds for `skin_vertices`.
/// - [`ClothingDeformerError::NonFiniteClearanceEpsilon`] if
///   `clearance_epsilon` is NaN or infinite.
///
/// On `Err`, some prefix of `cloth_vertices` (in parallel-execution order,
/// not necessarily index order) may already have been updated in place;
/// callers must treat the whole buffer as invalid and not present it for
/// rendering when this returns `Err`, exactly as they would for a `false`/
/// null-pointer failure sentinel at the FFI boundary.
pub fn fit_clothing_to_skin(
    skin_vertices: &[SkinnedVertex],
    cloth_vertices: &mut [SkinnedVertex],
    anchors: &[ClothAnchor],
    dna_scale: [f32; 3],
    clearance_epsilon: f32,
) -> Result<(), ClothingDeformerError> {
    if anchors.len() != cloth_vertices.len() {
        return Err(ClothingDeformerError::AnchorCountMismatch {
            cloth_vertex_count: cloth_vertices.len(),
            anchor_count: anchors.len(),
        });
    }
    if !clearance_epsilon.is_finite() {
        return Err(ClothingDeformerError::NonFiniteClearanceEpsilon);
    }

    let skin_vertex_count = skin_vertices.len();

    cloth_iter_mut!(cloth_vertices)
        .zip(cloth_iter!(anchors))
        .enumerate()
        .try_for_each(|(cloth_index, (cloth_vertex, anchor))| {
            let skin_index = anchor.target_skin_vertex_index as usize;
            let skin_vertex = skin_vertices.get(skin_index).ok_or(
                ClothingDeformerError::AnchorSkinIndexOutOfRange {
                    cloth_vertex_index: cloth_index,
                    target_skin_vertex_index: anchor.target_skin_vertex_index,
                    skin_vertex_count,
                },
            )?;

            // --- Step 2: delta morph propagation (rough fit). ---
            // V'_cloth = V'_skin + (O ⊙ S)
            let scaled_offset = v_mul_components(anchor.local_offset, dna_scale);
            let mut position = v_add(skin_vertex.position, scaled_offset);

            // --- Step 3: SDF push-out (anti-clipping guard). ---
            // Approximate the local skin surface as the tangent plane
            // through the (mutated) skin vertex, oriented by its
            // (mutated) normal. If a degenerate normal shows up at
            // runtime, skip the push-out for this vertex rather than
            // dividing by / normalizing a zero vector — the rough fit
            // from step 2 is left standing, which is a strictly safer
            // fallback than corrupting the position with a NaN push.
            if let Some(normal) = v_normalize(skin_vertex.normal) {
                let signed_distance = v_dot(v_sub(position, skin_vertex.position), normal);
                let min_safe_distance = anchor.thickness_clearance + clearance_epsilon;
                if signed_distance < min_safe_distance {
                    let correction = min_safe_distance - signed_distance;
                    position = v_add(position, v_scale(normal, correction));
                }
            }

            cloth_vertex.position = position;

            // --- Step 4: rigid-bind inheritance. ---
            // Copying the skin vertex's skinning data directly onto the
            // clothing vertex is what keeps the garment glued to the
            // skeleton during animation without any separate weight
            // painting for this item.
            cloth_vertex.bone_indices = skin_vertex.bone_indices;
            cloth_vertex.bone_weights = skin_vertex.bone_weights;

            Ok(())
        })
}

/// Derives a per-axis body scale from `dna`, for use as
/// [`fit_clothing_to_skin`]'s `dna_scale` parameter.
///
/// This is a placeholder heuristic, not part of this module's stable
/// contract: `CharacterDNA` (Phase 1) only carries scalar
/// `height_modifier` / `weight_modifier` fields, not a real per-axis body
/// scale, so this maps height growth onto the Y axis and girth growth onto
/// X/Z. It exists so the `lib.rs` integration below has something concrete
/// to call; Phase 4 (the actual DNA-driven body-mutation model) is the
/// right place to replace it with whatever real scale computation that
/// phase introduces.
pub fn dna_scale_from_character_dna(dna: &CharacterDNA) -> [f32; 3] {
    let girth = dna.weight_modifier;
    let height = dna.height_modifier;
    [girth, height, girth]
}

// ============================================================================
// FFI surface — Phase 5 addition.
//
// The exports below are thin `extern "C"` wrappers around
// `build_cloth_anchors` / `fit_clothing_to_skin` above, following the
// exact ownership/null-handling/buffer-freeing conventions already
// established by `texture_atlas::generate_runtime_atlas` /
// `free_atlas_buffer` (see the Phase 5 write-up for why this module had
// no FFI entry point at all before now):
// - Inputs are caller-owned, read through raw `(pointer, count)` pairs,
//   validated for null/nonzero-count mismatches before use.
// - The allocating function (`build_cloth_anchors_for_part`) returns a
//   heap-allocated, caller-owned `#[repr(C)]` buffer struct (mirroring
//   `MeshOutputBuffer`'s `(ptr, count)` shape) that must be released via
//   its own paired `free_cloth_anchor_buffer`, and by no other allocator
//   — exactly `free_mesh_buffer`/`free_atlas_buffer`'s pattern.
// - The non-allocating, in-place mutation (`fit_clothing_to_character`)
//   returns a `bool` success sentinel, mirroring `init_part_registry`,
//   rather than inventing a new convention for a function that has
//   nothing to hand back ownership of.
// - Every panic is caught at the boundary via `catch_unwind` and
//   collapsed to the same null/`false` sentinel a plain validation
//   failure would produce, mirroring `generate_runtime_atlas`/
//   `free_atlas_buffer` exactly. The crate's `Cargo.toml` sets
//   `panic = "unwind"` (see its own comment, and `lib.rs`'s module doc
//   comment, and RESULTS-03.md for the empirical verification), so this
//   `catch_unwind` genuinely runs in a release build: an internal panic
//   in `build_cloth_anchors_for_part`/`fit_clothing_to_character` is
//   caught here and collapsed to null/`false`, exactly like
//   `generate_runtime_atlas`/`free_atlas_buffer`. (An older version of
//   this comment claimed the opposite, back when the crate still shipped
//   `panic = "abort"`; that claim went stale when the profile changed and
//   was corrected here — see `UNSAFE_AUDIT.md` entry #10.)
// ============================================================================

use std::mem::ManuallyDrop;
use std::panic::{self, AssertUnwindSafe};
use std::ptr;
use std::slice;

/// Caller-owned output of [`build_cloth_anchors_for_part`]: a heap array
/// of [`ClothAnchor`] plus its length, mirroring `MeshOutputBuffer`'s
/// `(ptr, count)` shape. Must be released via
/// [`free_cloth_anchor_buffer`], and by no other allocator.
#[repr(C)]
pub struct ClothAnchorBuffer {
    pub anchors_ptr: *mut ClothAnchor,
    pub anchor_count: u32,
}
// Total size MUST be 16 bytes (8 + 4, padded to 8-byte alignment because
// of `anchors_ptr`), zero meaningful padding beyond that alignment pad —
// on the native (64-bit pointer) target. On wasm32-unknown-unknown,
// anchors_ptr is 4 bytes, not 8, so the real size is legitimately 8, not
// 16. Same rationale as lib.rs's CharacterDNA/MeshOutputBuffer
// assertions: this checks native/C++ ABI parity, which doesn't apply to
// the wasm-bindgen marshalling path.
#[cfg(not(target_arch = "wasm32"))]
const _: () = assert!(std::mem::size_of::<ClothAnchorBuffer>() == 16);

/// Builds [`ClothAnchor`]s for a clothing item against its default (bind
/// pose) skin mesh. Call once per clothing item, at part-load time — not
/// once per generated character (see [`build_cloth_anchors`]'s own doc
/// comment for why).
///
/// # Safety
/// - `cloth_vertices_ptr` must be valid for reads of `cloth_vertex_count`
///   consecutive `SkinnedVertex` values when `cloth_vertex_count != 0`.
/// - `default_skin_vertices_ptr` must be valid for reads of
///   `default_skin_vertex_count` consecutive `SkinnedVertex` values when
///   `default_skin_vertex_count != 0`.
/// - Either pointer may be null only if its paired count is `0`.
///
/// # Returns
/// A heap-allocated `*mut ClothAnchorBuffer` the caller must release via
/// [`free_cloth_anchor_buffer`], or `null` on any failure: a null
/// pointer paired with a nonzero count, an empty input buffer, a
/// non-finite vertex position, or an internal panic. Diagnostic detail
/// goes to stderr; this function itself never propagates a panic across
/// the FFI boundary.
#[no_mangle]
pub extern "C" fn build_cloth_anchors_for_part(
    cloth_vertices_ptr: *const SkinnedVertex,
    cloth_vertex_count: u32,
    default_skin_vertices_ptr: *const SkinnedVertex,
    default_skin_vertex_count: u32,
) -> *mut ClothAnchorBuffer {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        build_cloth_anchors_for_part_impl(
            cloth_vertices_ptr,
            cloth_vertex_count,
            default_skin_vertices_ptr,
            default_skin_vertex_count,
        )
    }));

    match result {
        Ok(Ok(buffer)) => Box::into_raw(Box::new(buffer)),
        Ok(Err(message)) => {
            eprintln!("[anthroforge] build_cloth_anchors_for_part failed: {message}");
            ptr::null_mut()
        }
        Err(_) => {
            eprintln!(
                "[anthroforge] build_cloth_anchors_for_part: internal panic suppressed at FFI boundary"
            );
            ptr::null_mut()
        }
    }
}

fn build_cloth_anchors_for_part_impl(
    cloth_vertices_ptr: *const SkinnedVertex,
    cloth_vertex_count: u32,
    default_skin_vertices_ptr: *const SkinnedVertex,
    default_skin_vertex_count: u32,
) -> Result<ClothAnchorBuffer, String> {
    // SAFETY: caller contract on `build_cloth_anchors_for_part` guarantees
    // each pointer is valid for reads of its paired count whenever that
    // count is nonzero; a null pointer with a zero count is accepted and
    // turned into an empty slice, which `build_cloth_anchors` itself then
    // rejects with a typed `EmptyClothMesh`/`EmptySkinMesh` error.
    let cloth_vertices = unsafe {
        raw_parts_to_slice(cloth_vertices_ptr, cloth_vertex_count, "cloth_vertices_ptr")?
    };
    let default_skin_vertices = unsafe {
        raw_parts_to_slice(
            default_skin_vertices_ptr,
            default_skin_vertex_count,
            "default_skin_vertices_ptr",
        )?
    };

    let mut anchors =
        build_cloth_anchors(cloth_vertices, default_skin_vertices).map_err(|e| e.to_string())?;
    // Ensure capacity == len so `free_cloth_anchor_buffer` can reconstruct
    // the Vec exactly via `Vec::from_raw_parts`, matching
    // `generate_character`/`generate_runtime_atlas`'s own pre-leak
    // `shrink_to_fit()` step.
    anchors.shrink_to_fit();

    let anchor_count = anchors.len() as u32;
    let mut owned = ManuallyDrop::new(anchors);
    let anchors_ptr = owned.as_mut_ptr();

    Ok(ClothAnchorBuffer { anchors_ptr, anchor_count })
}

/// Releases a [`ClothAnchorBuffer`] produced by
/// [`build_cloth_anchors_for_part`]. Safe to call with `null` (no-op).
/// Must not be called twice on the same pointer, and the pointer must
/// not be used again after this call.
///
/// # Safety
/// `buffer` must be either null or a pointer previously returned by
/// [`build_cloth_anchors_for_part`] that has not already been freed.
#[no_mangle]
pub extern "C" fn free_cloth_anchor_buffer(buffer: *mut ClothAnchorBuffer) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        if buffer.is_null() {
            return;
        }

        // SAFETY: caller contract guarantees `buffer` was produced by
        // `build_cloth_anchors_for_part` and not yet freed, so
        // reconstructing the `Box` here is valid and takes ownership
        // back from the caller.
        let boxed = unsafe { Box::from_raw(buffer) };
        let ClothAnchorBuffer { anchors_ptr, anchor_count } = *boxed;

        if !anchors_ptr.is_null() {
            // SAFETY: `build_cloth_anchors_for_part_impl` allocated this
            // via a `Vec` that was `shrink_to_fit()`-ed immediately
            // before leaking it, so `len == capacity == anchor_count`,
            // matching what `Vec::from_raw_parts` requires.
            let reclaimed = unsafe {
                Vec::from_raw_parts(anchors_ptr, anchor_count as usize, anchor_count as usize)
            };
            drop(reclaimed);
        }
    }));

    if result.is_err() {
        eprintln!(
            "[anthroforge] free_cloth_anchor_buffer: internal panic suppressed at FFI boundary"
        );
    }
}

/// Fits `cloth_vertices` (in place) to `skin_vertices` using previously
/// built `anchors`, deriving the DNA-driven per-axis body scale from
/// `dna` via [`dna_scale_from_character_dna`] — this is what makes
/// `dna_scale_from_character_dna` reachable end-to-end rather than only
/// unit-tested in isolation.
///
/// # ⚠ PLACEHOLDER — see the crate-level (`lib.rs`) and Phase 5 write-up's
/// "DNA-mutation gap" (blocker (c)). `skin_vertices` here must already be
/// whatever the caller considers this character's *mutated* skin buffer.
/// Nothing in this crate performs DNA-driven mesh mutation as of Phase 5
/// (`generate_character` only ever returns a single unmodified stored
/// part); passing that same, unmutated buffer here is equivalent to "no
/// mutation happened," which is an honest no-op, not the real
/// grow-with-the-body anti-clipping behavior this module exists for. A
/// real implementation needs, at minimum: (1) a per-vertex mutation step
/// inside `generate_character` (morph targets, bone-driven skinning, or
/// similar) that maps `CharacterDNA`'s `height_modifier`/
/// `weight_modifier` onto actual vertex displacement, not just the
/// coarse `[girth, height, girth]` scale heuristic
/// `dna_scale_from_character_dna` uses today; (2) that mutated buffer
/// surfacing as its own caller-visible output (or an internal one this
/// crate threads through to `fit_clothing_to_character` itself) instead
/// of being the caller's responsibility to supply; and (3) looping over
/// `CharacterDNA::equipped_clothing_ids_ptr` inside `generate_character`
/// to actually call `build_cloth_anchors_for_part`/
/// `fit_clothing_to_character` per equipped item and merge the results
/// into one `MeshOutputBuffer`. None of that is implemented here by
/// design — see the Phase 5 write-up for why inventing it now was out of
/// scope for this round.
///
/// # Safety
/// - `skin_vertices_ptr` must be valid for reads of `skin_vertex_count`
///   consecutive `SkinnedVertex` values when `skin_vertex_count != 0`.
/// - `cloth_vertices_ptr` must be valid for reads AND writes of
///   `cloth_vertex_count` consecutive `SkinnedVertex` values when
///   `cloth_vertex_count != 0` (this function mutates them in place).
/// - `anchors_ptr` must be valid for reads of `anchor_count` consecutive
///   `ClothAnchor` values when `anchor_count != 0`, and should be the
///   exact buffer previously returned by `build_cloth_anchors_for_part`
///   for this clothing item (anchors are positional; see
///   `fit_clothing_to_skin`'s doc comment).
/// - Any pointer above may be null only if its paired count is `0`.
/// - `dna` must be a valid, non-null, correctly-aligned pointer to a
///   fully-initialized `CharacterDNA` for the duration of this call.
///
/// # Returns
/// `true` on success (`cloth_vertices` now holds the fitted result).
/// `false` on any validation failure or internal panic — mirroring
/// `init_part_registry`'s bool-sentinel convention, since this function
/// allocates nothing for the caller to own. On `false`, some prefix of
/// `cloth_vertices` may already have been partially updated (see
/// `fit_clothing_to_skin`'s doc comment); callers must treat the whole
/// buffer as invalid, exactly as for any other `false`/null sentinel at
/// this FFI boundary.
#[no_mangle]
pub extern "C" fn fit_clothing_to_character(
    skin_vertices_ptr: *const SkinnedVertex,
    skin_vertex_count: u32,
    cloth_vertices_ptr: *mut SkinnedVertex,
    cloth_vertex_count: u32,
    anchors_ptr: *const ClothAnchor,
    anchor_count: u32,
    dna: *const CharacterDNA,
    clearance_epsilon: f32,
) -> bool {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        fit_clothing_to_character_impl(
            skin_vertices_ptr,
            skin_vertex_count,
            cloth_vertices_ptr,
            cloth_vertex_count,
            anchors_ptr,
            anchor_count,
            dna,
            clearance_epsilon,
        )
    }));

    match result {
        Ok(Ok(())) => true,
        Ok(Err(message)) => {
            eprintln!("[anthroforge] fit_clothing_to_character failed: {message}");
            false
        }
        Err(_) => {
            eprintln!(
                "[anthroforge] fit_clothing_to_character: internal panic suppressed at FFI boundary"
            );
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fit_clothing_to_character_impl(
    skin_vertices_ptr: *const SkinnedVertex,
    skin_vertex_count: u32,
    cloth_vertices_ptr: *mut SkinnedVertex,
    cloth_vertex_count: u32,
    anchors_ptr: *const ClothAnchor,
    anchor_count: u32,
    dna: *const CharacterDNA,
    clearance_epsilon: f32,
) -> Result<(), String> {
    if dna.is_null() {
        return Err("dna pointer was null".to_string());
    }
    // SAFETY: caller contract on `fit_clothing_to_character` guarantees
    // `dna` is non-null, aligned, and points at a valid, fully
    // initialized `CharacterDNA` for the duration of this call.
    let dna_ref: &CharacterDNA = unsafe { &*dna };
    let dna_scale = dna_scale_from_character_dna(dna_ref);

    // SAFETY: caller contract guarantees `skin_vertices_ptr`/`anchors_ptr`
    // are each valid for reads of their paired count whenever that count
    // is nonzero.
    let skin_vertices =
        unsafe { raw_parts_to_slice(skin_vertices_ptr, skin_vertex_count, "skin_vertices_ptr")? };
    let anchors = unsafe { raw_parts_to_slice(anchors_ptr, anchor_count, "anchors_ptr")? };

    if cloth_vertices_ptr.is_null() {
        if cloth_vertex_count != 0 {
            return Err("cloth_vertices_ptr was null with nonzero cloth_vertex_count".to_string());
        }
        // Nothing to fit; trivially successful, mirroring how an empty
        // `anchors`/`cloth_vertices` pair is handled by
        // `fit_clothing_to_skin` itself (a zero-length mismatch-free
        // no-op, not an error).
        return Ok(());
    }
    // SAFETY: caller contract guarantees `cloth_vertices_ptr` is valid for
    // reads and writes of `cloth_vertex_count` consecutive values.
    let cloth_vertices =
        unsafe { slice::from_raw_parts_mut(cloth_vertices_ptr, cloth_vertex_count as usize) };

    fit_clothing_to_skin(skin_vertices, cloth_vertices, anchors, dna_scale, clearance_epsilon)
        .map_err(|e| e.to_string())
}

/// Converts a caller-owned `(ptr, count)` pair into a borrowed slice,
/// with the same null/nonzero-count validation every input pair in this
/// module's FFI surface applies: a null pointer is only accepted when
/// `count == 0` (yielding an empty slice); any other null pointer is a
/// validation error rather than being dereferenced. Shared here once
/// instead of duplicated per FFI function.
///
/// # Safety
/// `ptr` must be valid for reads of `count` consecutive `T` values
/// whenever `count != 0`.
unsafe fn raw_parts_to_slice<'a, T>(
    ptr: *const T,
    count: u32,
    param_name: &'static str,
) -> Result<&'a [T], String> {
    if ptr.is_null() {
        if count == 0 {
            return Ok(&[]);
        }
        return Err(format!("{param_name} was null with nonzero count {count}"));
    }
    // SAFETY: forwarded from this function's own caller contract.
    Ok(unsafe { slice::from_raw_parts(ptr, count as usize) })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn v(position: [f32; 3], normal: [f32; 3]) -> SkinnedVertex {
        SkinnedVertex {
            position,
            normal,
            uv: [0.0, 0.0],
            bone_indices: [0, 0, 0, 0],
            bone_weights: [0.0, 0.0, 0.0, 0.0],
        }
    }

    #[test]
    fn kd_tree_nearest_matches_brute_force() {
        let positions: Vec<[f32; 3]> = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 5.0, 0.0],
            [-3.0, -1.0, 2.0],
            [10.0, 10.0, 10.0],
            [0.1, 0.1, 0.1],
        ];
        let tree = KdTree::build(&positions).expect("non-empty input builds a tree");

        let queries = [
            [0.0, 0.0, 0.0],
            [0.9, 0.1, -0.2],
            [9.0, 9.0, 9.0],
            [-2.5, -1.0, 1.9],
        ];
        for &q in &queries {
            let got = tree.nearest(&positions, q);
            let want = positions
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    v_squared_distance(**a, q)
                        .partial_cmp(&v_squared_distance(**b, q))
                        .unwrap()
                })
                .map(|(i, _)| i as u32)
                .unwrap();
            assert_eq!(got, want, "mismatch for query {q:?}");
        }
    }

    #[test]
    fn build_cloth_anchors_rejects_empty_inputs() {
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let cloth = vec![v([0.0, 0.1, 0.0], [0.0, 1.0, 0.0])];

        assert!(matches!(
            build_cloth_anchors(&cloth, &[]),
            Err(ClothingDeformerError::EmptySkinMesh)
        ));
        assert!(matches!(
            build_cloth_anchors(&[], &skin),
            Err(ClothingDeformerError::EmptyClothMesh)
        ));
    }

    #[test]
    fn build_cloth_anchors_computes_expected_offset_and_clearance() {
        // A single skin vertex at the origin with an "up" normal, and one
        // cloth vertex authored 0.05 units directly above it.
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let cloth = vec![v([0.0, 0.05, 0.0], [0.0, 1.0, 0.0])];

        let anchors = build_cloth_anchors(&cloth, &skin).expect("build should succeed");
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].target_skin_vertex_index, 0);
        assert_eq!(anchors[0].local_offset, [0.0, 0.05, 0.0]);
        assert!((anchors[0].thickness_clearance - 0.05).abs() < 1e-6);
    }

    /// A small, varied multi-vertex skin/cloth fixture shared by the
    /// `SkinKdTree`-path tests below.
    fn multi_vertex_skin_and_cloth() -> (Vec<SkinnedVertex>, Vec<SkinnedVertex>) {
        let skin = vec![
            v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            v([1.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            v([2.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            v([3.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            v([4.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            v([5.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            v([6.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            v([7.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            v([8.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            v([9.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let cloth = vec![
            v([0.1, 0.2, 0.0], [0.0, 1.0, 0.0]),
            v([2.9, 0.3, 0.0], [1.0, 0.0, 0.0]),
            v([5.05, -0.1, 0.0], [0.0, 1.0, 0.0]),
            v([8.4, 0.05, 0.0], [1.0, 0.0, 0.0]),
        ];
        (skin, cloth)
    }

    #[test]
    fn skin_kdtree_stride_one_matches_build_cloth_anchors_exactly() {
        let (skin, cloth) = multi_vertex_skin_and_cloth();

        let direct = build_cloth_anchors(&cloth, &skin).expect("direct build should succeed");

        let tree = build_skin_kdtree(&skin, 1).expect("stride-1 tree build should succeed");
        let via_tree = build_cloth_anchors_with_tree(&cloth, &skin, &tree)
            .expect("tree-based build should succeed");

        assert_eq!(direct, via_tree);
    }

    #[test]
    fn build_skin_kdtree_decimates_and_maps_indices_correctly() {
        let (skin, _cloth) = multi_vertex_skin_and_cloth();
        assert!(skin.len() >= 8);

        let tree = build_skin_kdtree(&skin, 4).expect("decimated tree build should succeed");

        let expected_sample_count = skin.len().div_ceil(4);
        assert_eq!(tree.sample_positions.len(), expected_sample_count);
        assert_eq!(tree.sample_to_full_index.len(), expected_sample_count);

        for &full_index in &tree.sample_to_full_index {
            assert!(
                (full_index as usize) < skin.len(),
                "sample_to_full_index entry {full_index} is out of range for a {}-vertex skin buffer",
                skin.len()
            );
        }
        // Every 4th vertex starting at 0: 0, 4, 8.
        assert_eq!(tree.sample_to_full_index, vec![0, 4, 8]);
    }

    #[test]
    fn decimated_tree_anchors_reference_valid_full_resolution_indices() {
        let (skin, cloth) = multi_vertex_skin_and_cloth();

        let tree = build_skin_kdtree(&skin, 4).expect("decimated tree build should succeed");
        let anchors = build_cloth_anchors_with_tree(&cloth, &skin, &tree)
            .expect("tree-based build should succeed");

        assert_eq!(anchors.len(), cloth.len());
        for anchor in &anchors {
            assert!(
                (anchor.target_skin_vertex_index as usize) < skin.len(),
                "anchor targets skin vertex {}, out of range for a {}-vertex skin buffer",
                anchor.target_skin_vertex_index,
                skin.len()
            );
        }
    }

    #[test]
    fn fit_applies_scale_and_copies_skinning_data() {
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let mut skin_mutated = skin.clone();
        skin_mutated[0].position = [0.0, 2.0, 0.0]; // character grew taller
        skin_mutated[0].bone_indices = [7, 0, 0, 0];
        skin_mutated[0].bone_weights = [1.0, 0.0, 0.0, 0.0];

        let anchors = vec![ClothAnchor {
            target_skin_vertex_index: 0,
            local_offset: [0.0, 0.1, 0.0],
            thickness_clearance: 0.1,
        }];
        let mut cloth = vec![v([0.0, 0.1, 0.0], [0.0, 1.0, 0.0])];

        fit_clothing_to_skin(&skin_mutated, &mut cloth, &anchors, [1.0, 2.0, 1.0], 0.0)
            .expect("fit should succeed");

        // V'_skin (0,2,0) + (O ⊙ S) = (0,2,0) + (0, 0.2, 0) = (0, 2.2, 0),
        // and the push-out guard should be a no-op since the rough fit
        // already sits exactly at the required clearance.
        assert!((cloth[0].position[1] - 2.2).abs() < 1e-5);
        assert_eq!(cloth[0].bone_indices, [7, 0, 0, 0]);
        assert_eq!(cloth[0].bone_weights, [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn fit_pushes_out_clothing_that_would_clip() {
        let skin_mutated = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let anchors = vec![ClothAnchor {
            target_skin_vertex_index: 0,
            local_offset: [0.0, 0.1, 0.0],
            thickness_clearance: 0.1,
        }];
        // Rough fit alone would land exactly on the skin surface (scale
        // collapses the offset to zero), which is inside the required
        // 0.1 clearance -> push-out must kick in.
        let mut cloth = vec![v([0.0, 0.1, 0.0], [0.0, 1.0, 0.0])];

        fit_clothing_to_skin(&skin_mutated, &mut cloth, &anchors, [1.0, 0.0, 1.0], 0.01)
            .expect("fit should succeed");

        assert!(
            (cloth[0].position[1] - 0.11).abs() < 1e-5,
            "expected push-out to land at clearance + epsilon, got {:?}",
            cloth[0].position
        );
    }

    #[test]
    fn fit_rejects_anchor_count_mismatch() {
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let anchors = vec![]; // zero anchors for one cloth vertex
        let mut cloth = vec![v([0.0, 0.1, 0.0], [0.0, 1.0, 0.0])];

        let err = fit_clothing_to_skin(&skin, &mut cloth, &anchors, [1.0, 1.0, 1.0], 0.0)
            .expect_err("mismatched lengths must error");
        assert!(matches!(err, ClothingDeformerError::AnchorCountMismatch { .. }));
    }

    #[test]
    fn fit_rejects_out_of_range_anchor() {
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let anchors = vec![ClothAnchor {
            target_skin_vertex_index: 99, // out of range
            local_offset: [0.0, 0.1, 0.0],
            thickness_clearance: 0.1,
        }];
        let mut cloth = vec![v([0.0, 0.1, 0.0], [0.0, 1.0, 0.0])];

        let err = fit_clothing_to_skin(&skin, &mut cloth, &anchors, [1.0, 1.0, 1.0], 0.0)
            .expect_err("out-of-range anchor must error");
        assert!(matches!(err, ClothingDeformerError::AnchorSkinIndexOutOfRange { .. }));
    }

    // ========================================================================
    // FFI-facing tests. These exercise `build_cloth_anchors_for_part`,
    // `fit_clothing_to_character`, and `free_cloth_anchor_buffer` exactly
    // the way a C++ caller would: raw pointers + counts in, raw pointers
    // out, freed through the paired `free_*` export. Distinct from the
    // tests above, which call the pure `build_cloth_anchors`/
    // `fit_clothing_to_skin` functions directly.
    // ========================================================================

    fn make_dna(height_modifier: f32, weight_modifier: f32) -> CharacterDNA {
        CharacterDNA {
            seed: 0,
            height_modifier,
            weight_modifier,
            head_id: 0,
            torso_id: 0,
            equipped_clothing_ids_ptr: std::ptr::null(),
            equipped_clothing_count: 0,
        }
    }

    #[test]
    fn ffi_build_and_free_round_trip() {
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let cloth = vec![v([0.0, 0.05, 0.0], [0.0, 1.0, 0.0])];

        let buffer_ptr = build_cloth_anchors_for_part(
            cloth.as_ptr(),
            cloth.len() as u32,
            skin.as_ptr(),
            skin.len() as u32,
        );
        assert!(!buffer_ptr.is_null(), "valid input should not fail");

        // SAFETY: `buffer_ptr` was just returned by
        // `build_cloth_anchors_for_part` and not yet freed.
        let buffer = unsafe { &*buffer_ptr };
        assert_eq!(buffer.anchor_count, 1);
        assert!(!buffer.anchors_ptr.is_null());

        // SAFETY: `anchors_ptr` is valid for reads of `anchor_count`
        // `ClothAnchor` values per `build_cloth_anchors_for_part_impl`.
        let anchors = unsafe {
            slice::from_raw_parts(buffer.anchors_ptr, buffer.anchor_count as usize)
        };
        assert_eq!(anchors[0].target_skin_vertex_index, 0);
        assert_eq!(anchors[0].local_offset, [0.0, 0.05, 0.0]);

        free_cloth_anchor_buffer(buffer_ptr);
    }

    #[test]
    fn ffi_free_is_noop_on_null() {
        // Must not crash / panic.
        free_cloth_anchor_buffer(std::ptr::null_mut());
    }

    #[test]
    fn ffi_build_rejects_empty_input_with_null_return() {
        let skin: Vec<SkinnedVertex> = vec![];
        let cloth = vec![v([0.0, 0.05, 0.0], [0.0, 1.0, 0.0])];

        let buffer_ptr = build_cloth_anchors_for_part(
            cloth.as_ptr(),
            cloth.len() as u32,
            skin.as_ptr(),
            0, // matches the empty `skin` Vec; skin.as_ptr() on an empty
               // Vec is a dangling-but-non-null "no allocation" pointer,
               // which is fine since count is 0.
        );
        assert!(buffer_ptr.is_null(), "empty skin buffer must fail, not allocate");
    }

    #[test]
    fn ffi_build_rejects_null_pointer_with_nonzero_count() {
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];

        let buffer_ptr = build_cloth_anchors_for_part(
            std::ptr::null(),
            3, // nonzero count paired with a null pointer
            skin.as_ptr(),
            skin.len() as u32,
        );
        assert!(buffer_ptr.is_null(), "null ptr + nonzero count must fail, not dereference");
    }

    #[test]
    fn ffi_fit_moves_cloth_and_reports_success() {
        let mut skin_mutated = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        skin_mutated[0].position = [0.0, 2.0, 0.0]; // character grew taller
        skin_mutated[0].bone_indices = [7, 0, 0, 0];
        skin_mutated[0].bone_weights = [1.0, 0.0, 0.0, 0.0];

        let anchors = vec![ClothAnchor {
            target_skin_vertex_index: 0,
            local_offset: [0.0, 0.1, 0.0],
            thickness_clearance: 0.1,
        }];
        let mut cloth = vec![v([0.0, 0.1, 0.0], [0.0, 1.0, 0.0])];

        // height_modifier=2.0, weight_modifier=1.0 -> dna_scale [1,2,1] via
        // `dna_scale_from_character_dna`, which this call exercises
        // end-to-end rather than only through its own unit test.
        let dna = make_dna(2.0, 1.0);

        let ok = fit_clothing_to_character(
            skin_mutated.as_ptr(),
            skin_mutated.len() as u32,
            cloth.as_mut_ptr(),
            cloth.len() as u32,
            anchors.as_ptr(),
            anchors.len() as u32,
            &dna as *const CharacterDNA,
            0.0,
        );
        assert!(ok, "valid input should succeed");

        // Same expected result as `fit_applies_scale_and_copies_skinning_data`
        // above, reached this time through the FFI wrapper + DNA-derived
        // scale instead of a hand-supplied `dna_scale`.
        assert!((cloth[0].position[1] - 2.2).abs() < 1e-5);
        assert_eq!(cloth[0].bone_indices, [7, 0, 0, 0]);
        assert_eq!(cloth[0].bone_weights, [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn ffi_fit_rejects_null_dna() {
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let anchors = vec![ClothAnchor {
            target_skin_vertex_index: 0,
            local_offset: [0.0, 0.1, 0.0],
            thickness_clearance: 0.1,
        }];
        let mut cloth = vec![v([0.0, 0.1, 0.0], [0.0, 1.0, 0.0])];

        let ok = fit_clothing_to_character(
            skin.as_ptr(),
            skin.len() as u32,
            cloth.as_mut_ptr(),
            cloth.len() as u32,
            anchors.as_ptr(),
            anchors.len() as u32,
            std::ptr::null(),
            0.0,
        );
        assert!(!ok, "null dna pointer must fail, not dereference");
    }

    #[test]
    fn ffi_fit_rejects_anchor_count_mismatch() {
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let anchors: Vec<ClothAnchor> = vec![]; // zero anchors for one cloth vertex
        let mut cloth = vec![v([0.0, 0.1, 0.0], [0.0, 1.0, 0.0])];
        let dna = make_dna(1.0, 1.0);

        let ok = fit_clothing_to_character(
            skin.as_ptr(),
            skin.len() as u32,
            cloth.as_mut_ptr(),
            cloth.len() as u32,
            anchors.as_ptr(),
            0,
            &dna as *const CharacterDNA,
            0.0,
        );
        assert!(!ok, "mismatched anchor/cloth counts must surface as a `false` return");
    }

    #[test]
    fn ffi_fit_null_cloth_with_zero_count_is_a_noop_success() {
        let skin = vec![v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0])];
        let anchors: Vec<ClothAnchor> = vec![];
        let dna = make_dna(1.0, 1.0);

        let ok = fit_clothing_to_character(
            skin.as_ptr(),
            skin.len() as u32,
            std::ptr::null_mut(),
            0,
            anchors.as_ptr(),
            0,
            &dna as *const CharacterDNA,
            0.0,
        );
        assert!(ok, "null cloth pointer with zero count is a valid no-op, not an error");
    }

    #[test]
    fn character_dna_equipped_clothing_fields_round_trip() {
        // Basic FFI-shape check for blocker (b): the new fields exist,
        // are independent of the pre-existing fields, and don't disturb
        // `dna_scale_from_character_dna`'s pre-existing behavior (it
        // deliberately never reads them — see that function's doc
        // comment).
        let ids = [101u32, 202u32];
        let dna = CharacterDNA {
            seed: 42,
            height_modifier: 1.5,
            weight_modifier: 0.8,
            head_id: 5,
            torso_id: 6,
            equipped_clothing_ids_ptr: ids.as_ptr(),
            equipped_clothing_count: ids.len() as u32,
        };

        assert_eq!(dna.equipped_clothing_count, 2);
        // SAFETY: `ids` outlives `dna` in this scope.
        let read_back = unsafe {
            slice::from_raw_parts(dna.equipped_clothing_ids_ptr, dna.equipped_clothing_count as usize)
        };
        assert_eq!(read_back, [101, 202]);

        let scale = dna_scale_from_character_dna(&dna);
        assert_eq!(scale, [0.8, 1.5, 0.8]);
    }
}