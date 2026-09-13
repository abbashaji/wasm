//! `obj_loader` — fallback parser for static, non-glTF modular parts
//! supplied as `.obj` (+ companion `.mtl`) files.
//!
//! Unlike `gltf_loader`, `.obj` carries no skin at all: no joints, no
//! per-vertex weights. Every vertex this module produces is therefore
//! rigidly bound, at full weight, to a single fixed bone (see
//! [`OBJ_BIND_BONE_NAME`]) rather than smoothly skinned. That is the right
//! behavior for the kind of asset that ends up in this fallback path —
//! hard-surface props, accessories, or placeholder geometry — not a
//! limitation to work around.
//!
//! This module never panics. Every malformed-input case (bad OBJ syntax,
//! a non-triangle-count-divisible attribute stream, an index that runs
//! past the vertex it should index into, ...) is surfaced as an
//! `ObjLoadError` variant so the caller (ultimately an `extern "C"`
//! boundary in `lib.rs`) can convert it into a clean `Result`/`bool`
//! return instead of unwinding.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::SkinnedVertex;

/// The single master-skeleton bone name every `.obj` fallback part is
/// bound to, at weight 1.0, in bone slot 0.
///
/// OBJ has no equivalent of glTF's per-vertex `JOINTS_0`/`WEIGHTS_0`
/// accessors, so there is no per-part-local joint list for
/// `skeleton_resolver::resolve_bone_indices` to remap the way it remaps
/// glTF parts. Instead every `.obj` part uses this one fixed, well-known
/// name as its (single-entry) "local bone list", so the exact same
/// name-keyed resolution path in `skeleton_resolver` still applies
/// unmodified: `master_skeleton.json` MUST define a bone with this exact
/// name, or every `.obj` part will fail to load with
/// `SkeletonError::UnknownBone`.
///
/// This is intentional, not an oversight: silently picking an arbitrary
/// bone (e.g. "whatever has global index 0") when the master skeleton has
/// no bone named "root" would risk rigidly attaching a part to the wrong
/// joint with no diagnostic, which is exactly the class of silent-failure
/// this engine's conventions (see `skeleton_resolver`'s doc comment)
/// reject elsewhere. If a production rig uses a different name for its
/// root/base bone, change this constant to match — do not special-case
/// around a missing entry at the call site.
pub const OBJ_BIND_BONE_NAME: &str = "root";

/// All the ways loading a single `.obj` modular part can fail.
#[derive(Debug)]
pub enum ObjLoadError {
    /// The file could not be opened / parsed as OBJ at all (bad syntax,
    /// unreadable path, unsupported directive, ...).
    Parse { path: PathBuf, source: tobj::LoadError },
    /// The file parsed but contained zero models (e.g. an OBJ with only
    /// comments, or with every face stripped by triangulation because it
    /// degenerated to fewer than 3 vertices).
    NoModels { path: PathBuf },
    /// Every model in the file parsed to zero usable vertices.
    EmptyGeometry { path: PathBuf },
    /// A position/normal/texcoord attribute stream's length was not a
    /// valid multiple of its component count (3 for positions/normals, 2
    /// for texcoords) — malformed/truncated data from the parser.
    MalformedAttributeArray {
        path: PathBuf,
        model_index: usize,
        attribute: &'static str,
        len: usize,
    },
    /// An optional attribute stream (normals or texcoords) was present
    /// but did not have one entry per vertex.
    AttributeLengthMismatch {
        path: PathBuf,
        model_index: usize,
        attribute: &'static str,
        expected: usize,
        found: usize,
    },
    /// A face index referenced a vertex past the end of that model's own
    /// vertex list — malformed/corrupt part data. Checked explicitly so a
    /// bad index becomes a clean `Err` here instead of an out-of-bounds
    /// index silently reaching the renderer on the other side of the FFI
    /// boundary.
    IndexOutOfRange {
        path: PathBuf,
        model_index: usize,
        index: u32,
        vertex_count: usize,
    },
}

impl fmt::Display for ObjLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ObjLoadError::Parse { path, source } => {
                write!(f, "failed to parse '{}': {source}", path.display())
            }
            ObjLoadError::NoModels { path } => {
                write!(f, "'{}' contains no models", path.display())
            }
            ObjLoadError::EmptyGeometry { path } => write!(
                f,
                "'{}' contains models but they yielded zero usable vertices",
                path.display()
            ),
            ObjLoadError::MalformedAttributeArray {
                path,
                model_index,
                attribute,
                len,
            } => write!(
                f,
                "'{}' model[{model_index}] attribute '{attribute}' has {len} floats, which is not a whole number of vertices",
                path.display()
            ),
            ObjLoadError::AttributeLengthMismatch {
                path,
                model_index,
                attribute,
                expected,
                found,
            } => write!(
                f,
                "'{}' model[{model_index}] attribute '{attribute}' has {found} elements, expected {expected}",
                path.display()
            ),
            ObjLoadError::IndexOutOfRange {
                path,
                model_index,
                index,
                vertex_count,
            } => write!(
                f,
                "'{}' model[{model_index}] face index {index} is out of range for its {vertex_count}-vertex model",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ObjLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ObjLoadError::Parse { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The fully-parsed contents of a single `.obj` modular part, in exactly
/// the same shape `gltf_loader::LoadedMesh` produces (minus the local
/// bone-name list, since there isn't one — see [`OBJ_BIND_BONE_NAME`]).
pub struct LoadedObjMesh {
    /// Flat vertex buffer. Every vertex's `bone_indices`/`bone_weights`
    /// are already the placeholder rigid binding (slot 0, weight 1.0,
    /// local index 0) — `skeleton_resolver::resolve_bone_indices` still
    /// needs to run on this, exactly as for glTF parts, to remap that
    /// placeholder local index 0 to `OBJ_BIND_BONE_NAME`'s *global*
    /// master-skeleton index.
    pub vertices: Vec<SkinnedVertex>,
    /// Triangle-list index buffer, already offset so it indexes correctly
    /// into `vertices` across all models in the source file.
    pub indices: Vec<u32>,
}

/// Parse a single `.obj` file (and its companion `.mtl`, if any) into a
/// [`LoadedObjMesh`].
///
/// # Triangulation
/// Non-triangulated faces (quads, n-gons) are triangulated automatically
/// via `tobj`'s built-in fan triangulation (`LoadOptions::triangulate`),
/// rather than rejected. A fallback loader's whole purpose is to accept
/// modular parts a full pipeline hasn't necessarily cleaned up yet, and
/// fan triangulation is the standard, unsurprising choice for the convex
/// or near-planar polygons that hard-surface OBJ exports typically use.
///
/// # Materials / textures
/// `SkinnedVertex` has no material-index field at all, so a `.mtl` file
/// (or a texture it references) that is missing or fails to parse cannot
/// affect the geometry this loader produces. Rather than failing the
/// whole part over data this engine's vertex format doesn't consume, such
/// failures are logged to stderr and loading continues with geometry
/// only.
///
/// # Normals
/// If the OBJ file omits normals entirely, per-vertex smooth normals are
/// computed from the (already-triangulated) face winding rather than
/// treated as an error — a normal-free static prop is common enough in
/// practice that rejecting it outright would make this "fallback" loader
/// less permissive than the format it's meant to be a fallback for.
///
/// Never panics; every malformed-input case returns `Err(ObjLoadError)`.
pub fn load_obj_file(path: &Path) -> Result<LoadedObjMesh, ObjLoadError> {
    let load_options = tobj::LoadOptions {
        triangulate: true,
        single_index: true,
        ignore_points: true,
        ignore_lines: true,
    };

    let (models, materials_result) =
        tobj::load_obj(path, &load_options).map_err(|source| ObjLoadError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

    build_loaded_obj_mesh(models, materials_result, path)
}

/// Same parsing/validation as [`load_obj_file`], but from an in-memory
/// `.obj` buffer instead of a filesystem path — for targets with no
/// filesystem (`wasm32-unknown-unknown`; see the AnthroForge/Web spec
/// §4.2). No companion `.mtl` is available in this context; per
/// `load_obj_file`'s existing "Materials / textures" behavior that is
/// already non-fatal (this engine's `SkinnedVertex` carries no material
/// data), so the material loader closure below simply reports it missing
/// and loading continues with geometry only, exactly as it already does
/// for a real file with an unreadable `.mtl`.
///
/// Never panics; every malformed-input case returns `Err(ObjLoadError)`,
/// same as `load_obj_file`.
pub fn load_obj_bytes(bytes: &[u8]) -> Result<LoadedObjMesh, ObjLoadError> {
    let synthetic_path = Path::new("<embedded bytes>");
    let load_options = tobj::LoadOptions {
        triangulate: true,
        single_index: true,
        ignore_points: true,
        ignore_lines: true,
    };

    let mut reader: &[u8] = bytes;
    let (models, materials_result) =
        tobj::load_obj_buf(&mut reader, &load_options, |_mtl_path| {
            Err(tobj::LoadError::OpenFileFailed)
        })
        .map_err(|source| ObjLoadError::Parse {
            path: synthetic_path.to_path_buf(),
            source,
        })?;

    build_loaded_obj_mesh(models, materials_result, synthetic_path)
}

/// Shared post-parse validation and `SkinnedVertex`/index-buffer
/// construction for both [`load_obj_file`] and [`load_obj_bytes`]. `path`
/// is used only to label diagnostics in `ObjLoadError`/log output — for
/// the bytes-based caller it is a synthetic, non-filesystem placeholder,
/// never touched for I/O here.
fn build_loaded_obj_mesh(
    models: Vec<tobj::Model>,
    materials_result: Result<Vec<tobj::Material>, tobj::LoadError>,
    path: &Path,
) -> Result<LoadedObjMesh, ObjLoadError> {
    if models.is_empty() {
        return Err(ObjLoadError::NoModels {
            path: path.to_path_buf(),
        });
    }

    // Missing/unparsable companion .mtl (and any texture it references) is
    // intentionally non-fatal — see the "Materials / textures" doc section
    // above. `tobj` reports it via this inner `Result` rather than the
    // outer one specifically so callers can make that choice.
    if let Err(source) = materials_result {
        eprintln!(
            "[anthroforge] '{}': companion .mtl (or a texture it references) could not be loaded, continuing with geometry only: {source}",
            path.display()
        );
    }

    let mut vertices: Vec<SkinnedVertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    for (model_index, model) in models.iter().enumerate() {
        let mesh = &model.mesh;

        if mesh.positions.len() % 3 != 0 {
            return Err(ObjLoadError::MalformedAttributeArray {
                path: path.to_path_buf(),
                model_index,
                attribute: "position",
                len: mesh.positions.len(),
            });
        }
        let vertex_count = mesh.positions.len() / 3;

        // An empty model (e.g. a named group with no faces) contributes no
        // geometry; skip it rather than treat it as an error, since the
        // rest of the file may still be perfectly valid.
        if vertex_count == 0 {
            continue;
        }

        let has_normals = !mesh.normals.is_empty();
        if has_normals {
            if mesh.normals.len() % 3 != 0 {
                return Err(ObjLoadError::MalformedAttributeArray {
                    path: path.to_path_buf(),
                    model_index,
                    attribute: "normal",
                    len: mesh.normals.len(),
                });
            }
            let found = mesh.normals.len() / 3;
            if found != vertex_count {
                return Err(ObjLoadError::AttributeLengthMismatch {
                    path: path.to_path_buf(),
                    model_index,
                    attribute: "normal",
                    expected: vertex_count,
                    found,
                });
            }
        }

        let has_uvs = !mesh.texcoords.is_empty();
        if has_uvs {
            if mesh.texcoords.len() % 2 != 0 {
                return Err(ObjLoadError::MalformedAttributeArray {
                    path: path.to_path_buf(),
                    model_index,
                    attribute: "texcoord",
                    len: mesh.texcoords.len(),
                });
            }
            let found = mesh.texcoords.len() / 2;
            if found != vertex_count {
                return Err(ObjLoadError::AttributeLengthMismatch {
                    path: path.to_path_buf(),
                    model_index,
                    attribute: "texcoord",
                    expected: vertex_count,
                    found,
                });
            }
        }

        // Validate every face index against *this model's* vertex count
        // before doing anything else with it, so a corrupt index becomes
        // a clean `Err` rather than an out-of-bounds read reaching either
        // `compute_smooth_normals` below or, worse, the renderer on the
        // other side of the FFI boundary.
        for &index in &mesh.indices {
            if index as usize >= vertex_count {
                return Err(ObjLoadError::IndexOutOfRange {
                    path: path.to_path_buf(),
                    model_index,
                    index,
                    vertex_count,
                });
            }
        }

        let base_vertex = vertices.len() as u32;

        for i in 0..vertex_count {
            let position = [
                mesh.positions[i * 3],
                mesh.positions[i * 3 + 1],
                mesh.positions[i * 3 + 2],
            ];
            let normal = if has_normals {
                [
                    mesh.normals[i * 3],
                    mesh.normals[i * 3 + 1],
                    mesh.normals[i * 3 + 2],
                ]
            } else {
                // Placeholder; overwritten by `compute_smooth_normals`
                // below when `!has_normals`.
                [0.0, 0.0, 0.0]
            };
            let uv = if has_uvs {
                [mesh.texcoords[i * 2], mesh.texcoords[i * 2 + 1]]
            } else {
                [0.0, 0.0]
            };

            vertices.push(SkinnedVertex {
                position,
                normal,
                uv,
                // Placeholder rigid binding (requirement #2): bone slot 0
                // carries the full weight, local joint index 0. Every
                // other slot is explicitly zeroed — never left
                // uninitialized — so the weight sum is exactly 1.0, not 0.0.
                bone_indices: [0, 0, 0, 0],
                bone_weights: [1.0, 0.0, 0.0, 0.0],
            });
        }

        if !has_normals {
            // `mesh.indices` are still model-local here (not yet offset by
            // `base_vertex`), which is exactly what this needs since it
            // only touches the slice of `vertices` just pushed for this
            // model.
            compute_smooth_normals(&mut vertices[base_vertex as usize..], &mesh.indices);
        }

        indices.extend(mesh.indices.iter().map(|&idx| idx + base_vertex));
    }

    if vertices.is_empty() {
        return Err(ObjLoadError::EmptyGeometry {
            path: path.to_path_buf(),
        });
    }

    Ok(LoadedObjMesh { vertices, indices })
}

/// Compute per-vertex smooth normals (area-weighted by construction, since
/// the un-normalized cross product's magnitude is proportional to
/// triangle area) for a model that had no normals of its own.
///
/// `vertices` is the model-local vertex slice; `local_indices` is that
/// same model's triangle-list indices, not yet offset into any larger
/// combined buffer. Every index in `local_indices` is assumed already
/// validated (`< vertices.len()`) by the caller.
fn compute_smooth_normals(vertices: &mut [SkinnedVertex], local_indices: &[u32]) {
    let mut accumulated = vec![[0.0f32; 3]; vertices.len()];

    for triangle in local_indices.chunks_exact(3) {
        let (a, b, c) = (
            triangle[0] as usize,
            triangle[1] as usize,
            triangle[2] as usize,
        );
        // Defensive: the caller validates every index before calling this,
        // but a slice re-borrow bug elsewhere should degrade to "skip this
        // triangle's contribution" rather than panic.
        if a >= vertices.len() || b >= vertices.len() || c >= vertices.len() {
            continue;
        }

        let pa = vertices[a].position;
        let pb = vertices[b].position;
        let pc = vertices[c].position;
        let edge1 = subtract(pb, pa);
        let edge2 = subtract(pc, pa);
        let face_normal = cross(edge1, edge2);

        for &vertex_index in &[a, b, c] {
            accumulated[vertex_index][0] += face_normal[0];
            accumulated[vertex_index][1] += face_normal[1];
            accumulated[vertex_index][2] += face_normal[2];
        }
    }

    for (vertex, sum) in vertices.iter_mut().zip(accumulated.into_iter()) {
        let length = (sum[0] * sum[0] + sum[1] * sum[1] + sum[2] * sum[2]).sqrt();
        vertex.normal = if length > f32::EPSILON {
            [sum[0] / length, sum[1] / length, sum[2] / length]
        } else {
            // Every incident face was degenerate (zero area) — there is no
            // meaningful direction to normalize. An arbitrary but
            // well-defined "up" is preferable to NaN/inf reaching a shader.
            [0.0, 1.0, 0.0]
        };
    }
}

fn subtract(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
