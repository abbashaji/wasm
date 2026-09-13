//! Phase 9a bench support: deterministic, programmatically-generated
//! realistic-scale synthetic assets (no hand-authored vertex lists, no
//! real randomness — see `PHASE_9A_TASK_SPEC.md` section 2).
//!
//! Every geometry function here is a pure function of its numeric
//! parameters (ring/segment counts, radii, heights) — the same call
//! always produces the same mesh, so benchmark runs are reproducible and
//! comparable across runs, machines, and re-runs of `cargo bench`.
//!
//! This module does not reuse or touch `sample-assets/test-fixtures/`
//! (the real 8-vertex cube fixtures every existing `#[cfg(test)]` uses) —
//! see the task spec's "Boundaries" section. Everything here writes into
//! a fresh temp directory created at bench-run time.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use anthroforge_core::{CharacterDNA, SkinnedVertex};

// ============================================================================
// Deterministic procedural geometry
// ============================================================================

/// A generated part's raw geometry: a flat vertex buffer plus a
/// triangle-list index buffer, in exactly the shape `.obj` needs
/// (`obj_loader`'s `single_index = true` convention — one aligned
/// position/normal/texcoord per vertex, already triangulated).
pub struct GeneratedMesh {
    pub vertices: Vec<SkinnedVertex>,
    pub indices: Vec<u32>,
}

impl GeneratedMesh {
    pub fn vertex_count(&self) -> usize {
        self.vertices.len()
    }
}

fn vertex(position: [f32; 3], normal: [f32; 3], uv: [f32; 2]) -> SkinnedVertex {
    SkinnedVertex {
        position,
        normal,
        uv,
        // Placeholder rigid binding — matches what `obj_loader::load_obj_file`
        // would itself assign for a part with no per-vertex skin data.
        // Only load-bearing for the direct (non-`.obj`-round-tripped)
        // decimation-stride sweep benchmark, which never goes through
        // `init_part_registry`/`skeleton_resolver` at all.
        bone_indices: [0, 0, 0, 0],
        bone_weights: [1.0, 0.0, 0.0, 0.0],
    }
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > f32::EPSILON {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

/// A subdivided UV sphere centered at the origin — used for the "head"
/// part. `rings` and `segments` control vertex count directly:
/// `2 + (rings - 1) * segments` vertices (two poles plus `rings - 1`
/// latitude bands of `segments` vertices each).
pub fn generate_uv_sphere(radius: f32, rings: usize, segments: usize) -> GeneratedMesh {
    assert!(rings >= 2 && segments >= 3);

    let mut vertices = Vec::with_capacity(2 + (rings - 1) * segments);
    let mut indices = Vec::new();

    let top_pole_index = 0u32;
    vertices.push(vertex([0.0, radius, 0.0], [0.0, 1.0, 0.0], [0.5, 0.0]));

    for ring in 1..rings {
        let theta = std::f32::consts::PI * (ring as f32) / (rings as f32);
        let y = radius * theta.cos();
        let r = radius * theta.sin();
        for seg in 0..segments {
            let phi = 2.0 * std::f32::consts::PI * (seg as f32) / (segments as f32);
            let x = r * phi.cos();
            let z = r * phi.sin();
            let position = [x, y, z];
            let normal = normalize(position);
            let uv = [seg as f32 / segments as f32, ring as f32 / rings as f32];
            vertices.push(vertex(position, normal, uv));
        }
    }

    let bottom_pole_index = vertices.len() as u32;
    vertices.push(vertex([0.0, -radius, 0.0], [0.0, -1.0, 0.0], [0.5, 1.0]));

    let first_ring_start = 1u32;
    for seg in 0..segments {
        let a = top_pole_index;
        let b = first_ring_start + seg as u32;
        let c = first_ring_start + ((seg + 1) % segments) as u32;
        indices.extend_from_slice(&[a, b, c]);
    }

    for ring in 0..(rings - 2) {
        let ring_start = first_ring_start + (ring * segments) as u32;
        let next_ring_start = first_ring_start + ((ring + 1) * segments) as u32;
        for seg in 0..segments {
            let seg_next = (seg + 1) % segments;
            let a = ring_start + seg as u32;
            let b = ring_start + seg_next as u32;
            let c = next_ring_start + seg as u32;
            let d = next_ring_start + seg_next as u32;
            indices.extend_from_slice(&[a, b, d]);
            indices.extend_from_slice(&[a, d, c]);
        }
    }

    let last_ring_start = first_ring_start + ((rings - 2) * segments) as u32;
    for seg in 0..segments {
        let a = bottom_pole_index;
        let b = last_ring_start + ((seg + 1) % segments) as u32;
        let c = last_ring_start + seg as u32;
        indices.extend_from_slice(&[a, b, c]);
    }

    GeneratedMesh { vertices, indices }
}

/// A subdivided, capped tube (cylinder-like) between `y_min` and `y_max`,
/// with radius `radius_fn(t)` at parametric height `t in [0, 1]` — used
/// for the "torso" part (near-constant radius) and, with a different
/// radius/height range, for the wrap-fit "clothing" parts (see
/// `generate_clothing_wrap` below).
///
/// `rings` is the number of latitude bands along the tube (so `rings + 1`
/// rings of `segments` vertices each), plus 2 more vertices for the flat
/// top/bottom cap centers: `(rings + 1) * segments + 2` vertices total.
pub fn generate_tube(
    y_min: f32,
    y_max: f32,
    radius_fn: impl Fn(f32) -> f32,
    rings: usize,
    segments: usize,
) -> GeneratedMesh {
    assert!(rings >= 1 && segments >= 3);

    let mut vertices = Vec::with_capacity((rings + 1) * segments + 2);
    let mut indices = Vec::new();

    for ring in 0..=rings {
        let t = ring as f32 / rings as f32;
        let y = y_min + (y_max - y_min) * t;
        let r = radius_fn(t);
        for seg in 0..segments {
            let phi = 2.0 * std::f32::consts::PI * (seg as f32) / (segments as f32);
            let x = r * phi.cos();
            let z = r * phi.sin();
            let position = [x, y, z];
            // Radial normal — a good-enough approximation for a mostly-
            // upright tube; exact tangent-plane correctness at the caps
            // is not required for this benchmark's purposes (realistic
            // vertex/triangle counts and non-degenerate positions, not
            // visual correctness — see the task spec).
            let normal = normalize([x, 0.0, z]);
            let uv = [seg as f32 / segments as f32, t];
            vertices.push(vertex(position, normal, uv));
        }
    }

    for ring in 0..rings {
        let ring_start = (ring * segments) as u32;
        let next_ring_start = ((ring + 1) * segments) as u32;
        for seg in 0..segments {
            let seg_next = (seg + 1) % segments;
            let a = ring_start + seg as u32;
            let b = ring_start + seg_next as u32;
            let c = next_ring_start + seg as u32;
            let d = next_ring_start + seg_next as u32;
            indices.extend_from_slice(&[a, b, d]);
            indices.extend_from_slice(&[a, d, c]);
        }
    }

    // Flat cap centers, fan-triangulated onto the bottom/top rings.
    let bottom_center_index = vertices.len() as u32;
    vertices.push(vertex([0.0, y_min, 0.0], [0.0, -1.0, 0.0], [0.5, 0.0]));
    for seg in 0..segments {
        let a = bottom_center_index;
        let b = seg as u32;
        let c = ((seg + 1) % segments) as u32;
        indices.extend_from_slice(&[a, c, b]);
    }

    let top_ring_start = (rings * segments) as u32;
    let top_center_index = vertices.len() as u32;
    vertices.push(vertex([0.0, y_max, 0.0], [0.0, 1.0, 0.0], [0.5, 1.0]));
    for seg in 0..segments {
        let a = top_center_index;
        let b = top_ring_start + seg as u32;
        let c = top_ring_start + ((seg + 1) % segments) as u32;
        indices.extend_from_slice(&[a, b, c]);
    }

    GeneratedMesh { vertices, indices }
}

/// A torso: a near-cylindrical capped tube, radius tapering very slightly
/// so it is not a perfect cylinder (avoids a perfectly-uniform-radius
/// degenerate case for the clothing anchor/KD-tree matching, per the
/// task spec's "non-degenerate geometry" requirement).
pub fn generate_torso(rings: usize, segments: usize) -> GeneratedMesh {
    generate_tube(
        -0.7,
        0.7,
        |t| 0.40 - 0.05 * (t - 0.5).abs(),
        rings,
        segments,
    )
}

/// A clothing item that wraps a sub-range of the torso's bounding
/// cylinder, at `radius_margin` beyond the torso's own radius at that
/// height — close enough to the skin surface that `fit_clothing_to_skin`'s
/// anchor matching has genuinely non-trivial work to do, per the task
/// spec.
pub fn generate_clothing_wrap(
    y_min: f32,
    y_max: f32,
    radius_margin: f32,
    rings: usize,
    segments: usize,
) -> GeneratedMesh {
    generate_tube(
        y_min,
        y_max,
        move |t| 0.40 - 0.05 * ((y_min + (y_max - y_min) * t).abs() / 0.7 - 0.5).abs() + radius_margin,
        rings,
        segments,
    )
}

// ============================================================================
// .obj serialization (matches sample-assets/test-fixtures' plain
// "v x y z" / "f i j k" convention exactly — no vn/vt lines; `obj_loader`
// computes its own smooth normals when normals are omitted, so this
// still exercises the exact same loader code path real fallback assets
// do).
// ============================================================================

pub fn write_obj(path: &Path, mesh: &GeneratedMesh) -> std::io::Result<()> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    for v in &mesh.vertices {
        writeln!(out, "v {} {} {}", v.position[0], v.position[1], v.position[2])?;
    }
    for tri in mesh.indices.chunks_exact(3) {
        // OBJ face indices are 1-based.
        writeln!(out, "f {} {} {}", tri[0] + 1, tri[1] + 1, tri[2] + 1)?;
    }
    out.flush()
}

const MASTER_SKELETON_JSON: &str = r#"{
  "root": 0,
  "pelvis": 1,
  "spine_01": 2,
  "spine_02": 3,
  "spine_03": 4,
  "neck_01": 5,
  "head": 6,
  "clavicle_l": 7,
  "upperarm_l": 8,
  "lowerarm_l": 9,
  "hand_l": 10,
  "clavicle_r": 11,
  "upperarm_r": 12,
  "lowerarm_r": 13,
  "hand_r": 14,
  "thigh_l": 15,
  "calf_l": 16,
  "foot_l": 17,
  "thigh_r": 18,
  "calf_r": 19,
  "foot_r": 20
}
"#;

// ============================================================================
// Full synthetic asset directory + one-time GLOBAL_REGISTRY init.
// ============================================================================

pub const SHIRT_ID: u32 = 80001;
pub const JACKET_ID: u32 = 80002;
pub const VEST_ID: u32 = 80003;

pub const FIXED_HEAD_ID: u32 = 90000;
pub const FIXED_TORSO_ID: u32 = 95000;

/// How many distinct, never-before-requested `(head_id, torso_id)` pairs
/// are pre-registered for `bench_generate_character_cold_cache` to draw
/// one fresh pair from per timed iteration. Sized generously above what
/// a `sample_size(10)` / short-`measurement_time` criterion configuration
/// actually consumes in practice (see PHASE_9A_RESULTS.md for the real
/// iteration counts observed) so the benchmark keeps measuring genuine
/// cold-cache cost for its entire run rather than quietly wrapping around
/// into warm-cache territory partway through.
pub const COLD_CACHE_POOL_SIZE: usize = 96;

pub struct BenchAssets {
    // Kept alive for the process lifetime so the temp directory (which
    // `init_part_registry` only reads from at init time, but which must
    // not be deleted out from under it) is never dropped mid-run.
    #[allow(dead_code)]
    pub asset_dir: PathBuf,
    pub fixed_head_vertex_count: usize,
    pub fixed_torso_vertex_count: usize,
    pub shirt_vertex_count: usize,
    pub jacket_vertex_count: usize,
    pub vest_vertex_count: usize,
    cold_pairs: Vec<(u32, u32)>,
    cold_cursor: AtomicUsize,
}

impl BenchAssets {
    /// Returns the next never-yet-requested `(head_id, torso_id)` pair for
    /// a genuine cold-cache `generate_character` call. Wraps around (with
    /// a one-time stderr warning) if `COLD_CACHE_POOL_SIZE` is ever
    /// exhausted by an unexpectedly high criterion iteration count —
    /// see `COLD_CACHE_POOL_SIZE`'s doc comment.
    pub fn next_cold_pair(&self) -> (u32, u32) {
        let i = self.cold_cursor.fetch_add(1, Ordering::Relaxed);
        if i == self.cold_pairs.len() {
            eprintln!(
                "[bench] WARNING: cold-cache pool of {} pairs exhausted after {} draws; \
                 further 'cold-cache' iterations are reusing already-warm (head, torso) pairs \
                 and no longer measure genuine cold-cache cost. Increase COLD_CACHE_POOL_SIZE.",
                self.cold_pairs.len(),
                i
            );
        }
        self.cold_pairs[i % self.cold_pairs.len()]
    }
}

fn unique_temp_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("anthroforge-phase9a-bench-{pid}-{n}"));
    std::fs::create_dir_all(&dir).expect("failed to create synthetic bench asset temp dir");
    dir
}

/// Builds every synthetic part this bench file needs, writes them as
/// `.obj` fixtures (plus `master_skeleton.json`) into a fresh temp
/// directory, and calls `init_part_registry` on it exactly once. Safe to
/// call from multiple benchmark functions — idempotent via `OnceLock`.
pub fn bench_assets() -> &'static BenchAssets {
    static ASSETS: OnceLock<BenchAssets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let dir = unique_temp_dir();

        std::fs::write(dir.join("master_skeleton.json"), MASTER_SKELETON_JSON)
            .expect("failed to write master_skeleton.json");

        // Head: ~2,500 vertices (within the task spec's 2,000-4,000
        // range) — one UV sphere, reused (byte-for-byte, just re-saved
        // under a different numeric id per file) for the fixed body and
        // for every cold-cache pool body. Real head assets in production
        // obviously differ per head_id; for a *cost* benchmark (not a
        // visual one) an identical mesh under a fresh id is exactly as
        // expensive to merge/mutate/tree-build as a genuinely different
        // one of the same vertex count would be.
        let head_mesh = generate_uv_sphere(0.5, 48, 53);
        // Torso: ~5,700 vertices (within 4,000-8,000).
        let torso_mesh = generate_torso(70, 80);

        let shirt_mesh = generate_clothing_wrap(0.0, 0.65, 0.02, 12, 80); // ~1,040 verts
        let jacket_mesh = generate_clothing_wrap(-0.1, 0.75, 0.05, 36, 80); // ~2,960 verts
        let vest_mesh = generate_clothing_wrap(0.05, 0.55, 0.03, 14, 80); // ~1,200 verts

        let fixed_head_vertex_count = head_mesh.vertex_count();
        let fixed_torso_vertex_count = torso_mesh.vertex_count();
        let shirt_vertex_count = shirt_mesh.vertex_count();
        let jacket_vertex_count = jacket_mesh.vertex_count();
        let vest_vertex_count = vest_mesh.vertex_count();

        write_obj(&dir.join(format!("{FIXED_HEAD_ID}_head_fixed.obj")), &head_mesh)
            .expect("write fixed head .obj");
        write_obj(&dir.join(format!("{FIXED_TORSO_ID}_torso_fixed.obj")), &torso_mesh)
            .expect("write fixed torso .obj");
        write_obj(&dir.join(format!("{SHIRT_ID}_shirt.obj")), &shirt_mesh).expect("write shirt .obj");
        write_obj(&dir.join(format!("{JACKET_ID}_jacket.obj")), &jacket_mesh).expect("write jacket .obj");
        write_obj(&dir.join(format!("{VEST_ID}_vest.obj")), &vest_mesh).expect("write vest .obj");

        let mut cold_pairs = Vec::with_capacity(COLD_CACHE_POOL_SIZE);
        for i in 0..COLD_CACHE_POOL_SIZE {
            let head_id = 91000 + i as u32;
            let torso_id = 96000 + i as u32;
            write_obj(&dir.join(format!("{head_id}_head_cold.obj")), &head_mesh)
                .expect("write cold-pool head .obj");
            write_obj(&dir.join(format!("{torso_id}_torso_cold.obj")), &torso_mesh)
                .expect("write cold-pool torso .obj");
            cold_pairs.push((head_id, torso_id));
        }

        let asset_dir_c = std::ffi::CString::new(dir.to_str().expect("temp dir path was not UTF-8"))
            .expect("temp dir path contained a NUL byte");

        // `init_part_registry` is `pub extern "C" fn` but not `unsafe
        // fn` (its safety contract is about the pointer's validity, not
        // the call itself), so no `unsafe` block is needed here — the
        // pointer comes from a live `CString` we hold, so the contract
        // is upheld regardless.
        let ok = anthroforge_core::init_part_registry(asset_dir_c.as_ptr());
        assert!(
            ok,
            "init_part_registry failed against the synthetic bench asset dir '{}'",
            dir.display()
        );

        BenchAssets {
            asset_dir: dir,
            fixed_head_vertex_count,
            fixed_torso_vertex_count,
            shirt_vertex_count,
            jacket_vertex_count,
            vest_vertex_count,
            cold_pairs,
            cold_cursor: AtomicUsize::new(0),
        }
    })
}

/// Regenerates a fresh head+torso merged vertex buffer directly in
/// memory (no `.obj` round-trip, no registry) — exactly what
/// `Registry::get_or_build_skin_tree` would hand to
/// `clothing_deformer::build_skin_kdtree` for the fixed
/// `(FIXED_HEAD_ID, FIXED_TORSO_ID)` body, at the same vertex counts used
/// everywhere else in this file. Used only by
/// `bench_skin_kdtree_decimation_stride`, which needs direct control over
/// `stride` (see `anthroforge_core::bench_only_build_skin_kdtree`) rather
/// than the registry's fixed-stride cached path.
pub fn generate_merged_skin_for_kdtree_bench() -> Vec<SkinnedVertex> {
    let head = generate_uv_sphere(0.5, 48, 53);
    let torso = generate_torso(70, 80);
    let mut merged = Vec::with_capacity(head.vertex_count() + torso.vertex_count());
    merged.extend(head.vertices);
    merged.extend(torso.vertices);
    merged
}

/// Builds a `CharacterDNA` for `(head_id, torso_id)` with `clothing_ids`
/// equipped. `clothing_ids` must outlive the returned `CharacterDNA` (it
/// borrows the slice's pointer) — callers keep the `Vec`/array alive
/// across the `generate_character` call.
pub fn make_dna(head_id: u32, torso_id: u32, clothing_ids: &[u32]) -> CharacterDNA {
    CharacterDNA {
        seed: 42,
        // Deliberately non-identity (matches the crate's own
        // `generate_character_composes_head_torso_and_fitted_clothing`
        // test convention) so DNA mutation + clothing fit do real,
        // non-trivial work every call rather than a degenerate identity
        // fast path — but mild, so the synthetic geometry never
        // collapses to something degenerate.
        height_modifier: 1.1,
        weight_modifier: 1.05,
        head_id,
        torso_id,
        equipped_clothing_ids_ptr: if clothing_ids.is_empty() {
            std::ptr::null()
        } else {
            clothing_ids.as_ptr()
        },
        equipped_clothing_count: clothing_ids.len() as u32,
    }
}

/// Calls `generate_character`, asserts success, and frees the returned
/// buffer — the common "call it and clean up" shape every benchmark
/// routine below needs. Returns the vertex count actually produced
/// (useful as a sanity check, `black_box`-consumed by callers so the
/// optimizer can't reason the call away).
pub fn call_generate_character(dna: &CharacterDNA) -> u32 {
    // `generate_character`/`free_mesh_buffer` are `pub extern "C" fn`,
    // not `unsafe fn` — only the pointer dereferences below need
    // `unsafe`. `dna` is a valid, non-null, aligned, fully-initialized
    // `CharacterDNA` borrowed from the caller's stack, matching
    // `generate_character`'s safety contract.
    let buffer_ptr = anthroforge_core::generate_character(dna as *const CharacterDNA);
    assert!(
        !buffer_ptr.is_null(),
        "generate_character returned null in a benchmark that expected success"
    );
    // SAFETY: just-returned, not-yet-freed pointer.
    let vertex_count = unsafe { (*buffer_ptr).vertices_count };
    // `buffer_ptr` was returned by `generate_character` above and has not
    // been freed yet; not used again after this call.
    anthroforge_core::free_mesh_buffer(buffer_ptr);
    vertex_count
}
