//! `gltf_loader` — parses `.gltf` / `.glb` files into flat, FFI-ready vertex
//! and index buffers.
//!
//! This module never panics. Every failure mode (malformed file, missing
//! accessor, unsupported primitive topology, mismatched attribute lengths,
//! an unnamed/unskinned joint, ...) is surfaced as a `GltfLoadError` variant
//! so the caller (ultimately an `extern "C"` boundary in `lib.rs`) can
//! convert it into a clean `Result`/`bool` return instead of unwinding.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::SkinnedVertex;

/// All the ways loading a single modular part can fail.
#[derive(Debug)]
pub enum GltfLoadError {
    /// The file could not be opened / parsed as glTF or GLB at all.
    Parse { path: PathBuf, source: gltf::Error },
    /// The document contained no meshes.
    NoMeshes { path: PathBuf },
    /// A primitive used a topology other than triangles (points, lines,
    /// strips, fans, ...). This engine only supports triangle lists.
    UnsupportedTopology {
        path: PathBuf,
        mesh_index: usize,
        primitive_index: usize,
        mode: gltf::mesh::Mode,
    },
    /// A required vertex attribute accessor was absent from a primitive.
    MissingAccessor {
        path: PathBuf,
        mesh_index: usize,
        primitive_index: usize,
        accessor: &'static str,
    },
    /// A primitive had no index accessor (this engine requires indexed
    /// geometry so vertices can be shared across triangles).
    MissingIndices {
        path: PathBuf,
        mesh_index: usize,
        primitive_index: usize,
    },
    /// Attribute arrays disagreed in length (e.g. fewer UVs than positions).
    AttributeLengthMismatch {
        path: PathBuf,
        mesh_index: usize,
        primitive_index: usize,
        attribute: &'static str,
        expected: usize,
        found: usize,
    },
    /// The mesh referenced by a node has no `skin` attached. Every modular
    /// part in Anthroforge must be skinned so it can be remapped onto the
    /// master skeleton.
    NoSkin { path: PathBuf, mesh_index: usize },
    /// A joint node in the skin's hierarchy has no `name`, so it can never
    /// be matched against `master_skeleton.json`.
    UnnamedJoint {
        path: PathBuf,
        joint_index: usize,
    },
    /// A buffer's data source (embedded GLB `BIN` chunk, external file, or
    /// `data:` URI) could not be resolved or read.
    BufferSource {
        path: PathBuf,
        buffer_index: usize,
        reason: String,
    },
    /// A resolved buffer had fewer bytes than the glTF document declares
    /// for it, so any accessor reading past that point would read out of
    /// bounds.
    BufferTooShort {
        path: PathBuf,
        buffer_index: usize,
        declared_len: usize,
        actual_len: usize,
    },
    /// A primitive's index accessor referenced a vertex past the end of
    /// that *same primitive's* own attribute arrays — malformed/corrupt
    /// part data. Checked explicitly so a bad index becomes a clean `Err`
    /// here instead of an out-of-bounds `indices` entry silently reaching
    /// the renderer on the other side of the FFI boundary (mirrors
    /// `obj_loader::ObjLoadError::IndexOutOfRange`'s existing check for the
    /// `.obj` path).
    IndexOutOfRange {
        path: PathBuf,
        mesh_index: usize,
        primitive_index: usize,
        index: u32,
        vertex_count: usize,
    },
}

impl fmt::Display for GltfLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GltfLoadError::Parse { path, source } => {
                write!(f, "failed to parse '{}': {source}", path.display())
            }
            GltfLoadError::NoMeshes { path } => {
                write!(f, "'{}' contains no meshes", path.display())
            }
            GltfLoadError::UnsupportedTopology {
                path,
                mesh_index,
                primitive_index,
                mode,
            } => write!(
                f,
                "'{}' mesh[{mesh_index}] primitive[{primitive_index}] uses unsupported topology {mode:?} (only Triangles is supported)",
                path.display()
            ),
            GltfLoadError::MissingAccessor {
                path,
                mesh_index,
                primitive_index,
                accessor,
            } => write!(
                f,
                "'{}' mesh[{mesh_index}] primitive[{primitive_index}] is missing required accessor '{accessor}'",
                path.display()
            ),
            GltfLoadError::MissingIndices {
                path,
                mesh_index,
                primitive_index,
            } => write!(
                f,
                "'{}' mesh[{mesh_index}] primitive[{primitive_index}] has no index accessor (non-indexed primitives are not supported)",
                path.display()
            ),
            GltfLoadError::AttributeLengthMismatch {
                path,
                mesh_index,
                primitive_index,
                attribute,
                expected,
                found,
            } => write!(
                f,
                "'{}' mesh[{mesh_index}] primitive[{primitive_index}] attribute '{attribute}' has {found} elements, expected {expected}",
                path.display()
            ),
            GltfLoadError::NoSkin { path, mesh_index } => write!(
                f,
                "'{}' mesh[{mesh_index}] has no skin attached to any referencing node; modular parts must be skinned",
                path.display()
            ),
            GltfLoadError::UnnamedJoint { path, joint_index } => write!(
                f,
                "'{}' skin joint[{joint_index}] has no node name and cannot be resolved against the master skeleton",
                path.display()
            ),
            GltfLoadError::BufferSource {
                path,
                buffer_index,
                reason,
            } => write!(
                f,
                "'{}' buffer[{buffer_index}] could not be loaded: {reason}",
                path.display()
            ),
            GltfLoadError::BufferTooShort {
                path,
                buffer_index,
                declared_len,
                actual_len,
            } => write!(
                f,
                "'{}' buffer[{buffer_index}] declares {declared_len} bytes but only {actual_len} were available",
                path.display()
            ),
            GltfLoadError::IndexOutOfRange {
                path,
                mesh_index,
                primitive_index,
                index,
                vertex_count,
            } => write!(
                f,
                "'{}' mesh[{mesh_index}] primitive[{primitive_index}] index accessor references vertex {index}, but this primitive only has {vertex_count} vertices",
                path.display()
            ),
        }
    }
}

impl std::error::Error for GltfLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GltfLoadError::Parse { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The fully-parsed, still skin-local (i.e. not yet remapped to the master
/// skeleton) contents of a single modular part.
pub struct LoadedMesh {
    /// Flat vertex buffer. `bone_indices` in each vertex are *local* joint
    /// indices (indices into `local_bone_names`), not yet global skeleton
    /// indices. `skeleton_resolver::resolve_bone_indices` performs that
    /// remap in place.
    pub vertices: Vec<SkinnedVertex>,
    /// Triangle-list index buffer, already offset so it indexes correctly
    /// into `vertices` across all primitives in the source mesh.
    pub indices: Vec<u32>,
    /// The joint node names of the skin, in the same order as the local
    /// `JOINTS_0` indices used by `vertices[..].bone_indices`.
    pub local_bone_names: Vec<String>,
    /// `local_joint_parents[i]` is the index (into this same
    /// `local_bone_names`/`local_joint_parents` array) of local joint
    /// `i`'s parent, or `None` if `i`'s true parent is not itself a
    /// member of this skin's joint list (either because `i` really is
    /// the skeleton's root, or because this part's skin starts partway
    /// down a chain whose upper joints live in a different part — you
    /// cannot tell which from this part's data alone; that's resolved by
    /// `skeleton_resolver::contribute_global_joints`, not here).
    pub local_joint_parents: Vec<Option<u32>>,
    /// `local_joint_bind_poses[i]` is local joint `i`'s bind-pose local
    /// transform (relative to its parent, or to the skin's own local
    /// space if it has none), decomposed to translation/rotation/scale
    /// regardless of whether the source glTF node authored it as TRS or
    /// as a raw matrix.
    pub local_joint_bind_poses: Vec<JointBindPose>,
}

/// A joint's bind-pose local transform (relative to its parent node, or
/// to the skin's own local space if it has none), decomposed to
/// translation/rotation/scale. Always present regardless of whether the
/// source glTF node authored its transform as TRS or as a raw matrix —
/// see [`decompose_trs_matrix`] for the latter case.
#[derive(Clone, Copy, Debug)]
pub struct JointBindPose {
    pub translation: [f32; 3],
    pub rotation: [f32; 4], // quaternion, (x, y, z, w)
    pub scale: [f32; 3],
}

/// Decomposes a raw column-major 4x4 glTF/GL-convention transform matrix
/// into translation/rotation/scale.
///
/// Standard TRS decomposition: the translation is the matrix's fourth
/// column; each axis's scale is the length of that axis's column (the
/// first three components); the 3x3 rotation matrix is recovered by
/// dividing each column by its own just-extracted scale (normalizing the
/// scale back out), then converting that orthonormal 3x3 matrix to a
/// quaternion.
///
/// A column whose length is (numerically) zero is left un-normalized
/// rather than divided by zero — a degenerate (zero-scale-on-one-axis)
/// authored transform is unusual but not this function's job to reject;
/// `gltf_loader`'s job is to extract exactly what's there.
pub fn decompose_trs_matrix(matrix: [[f32; 4]; 4]) -> JointBindPose {
    let translation = [matrix[3][0], matrix[3][1], matrix[3][2]];

    // Each `matrix[i]` is column `i`; its first three components are that
    // column's (possibly scaled) basis vector.
    let col0 = [matrix[0][0], matrix[0][1], matrix[0][2]];
    let col1 = [matrix[1][0], matrix[1][1], matrix[1][2]];
    let col2 = [matrix[2][0], matrix[2][1], matrix[2][2]];

    fn length(v: [f32; 3]) -> f32 {
        (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
    }

    let sx = length(col0);
    let sy = length(col1);
    let sz = length(col2);

    fn normalize_or_identity(v: [f32; 3], s: f32) -> [f32; 3] {
        if s.abs() > 1e-8 {
            [v[0] / s, v[1] / s, v[2] / s]
        } else {
            v
        }
    }

    let r0 = normalize_or_identity(col0, sx);
    let r1 = normalize_or_identity(col1, sy);
    let r2 = normalize_or_identity(col2, sz);

    // Row-major entries of the now-orthonormal 3x3 rotation matrix.
    // m[row][col]: column `c`'s components are `rC[row]`.
    let m00 = r0[0];
    let m10 = r0[1];
    let m20 = r0[2];
    let m01 = r1[0];
    let m11 = r1[1];
    let m21 = r1[2];
    let m02 = r2[0];
    let m12 = r2[1];
    let m22 = r2[2];

    // Standard (Shepperd's method) rotation-matrix-to-quaternion
    // conversion, branching on the trace to always take the square root
    // of a positive quantity.
    let trace = m00 + m11 + m22;
    let (qx, qy, qz, qw);
    if trace > 0.0 {
        let s = 0.5 / (trace + 1.0).sqrt();
        qw = 0.25 / s;
        qx = (m21 - m12) * s;
        qy = (m02 - m20) * s;
        qz = (m10 - m01) * s;
    } else if m00 > m11 && m00 > m22 {
        let s = 2.0 * (1.0 + m00 - m11 - m22).sqrt();
        qw = (m21 - m12) / s;
        qx = 0.25 * s;
        qy = (m01 + m10) / s;
        qz = (m02 + m20) / s;
    } else if m11 > m22 {
        let s = 2.0 * (1.0 + m11 - m00 - m22).sqrt();
        qw = (m02 - m20) / s;
        qx = (m01 + m10) / s;
        qy = 0.25 * s;
        qz = (m12 + m21) / s;
    } else {
        let s = 2.0 * (1.0 + m22 - m00 - m11).sqrt();
        qw = (m10 - m01) / s;
        qx = (m02 + m20) / s;
        qy = (m12 + m21) / s;
        qz = 0.25 * s;
    }

    JointBindPose {
        translation,
        rotation: [qx, qy, qz, qw],
        scale: [sx, sy, sz],
    }
}

/// Parse a single `.gltf` or `.glb` file into a [`LoadedMesh`].
///
/// Only the first mesh in the document is loaded (each Anthroforge part
/// file is expected to contain exactly one logical modular part), but all
/// of that mesh's primitives are read and concatenated, with index buffers
/// correctly offset. Never panics; every malformed-input case returns
/// `Err(GltfLoadError)`.
pub fn load_gltf_file(path: &Path) -> Result<LoadedMesh, GltfLoadError> {
    let gltf = gltf::Gltf::open(path).map_err(|source| GltfLoadError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    build_loaded_mesh(gltf.document, gltf.blob, path)
}

/// Parse a single `.gltf`/`.glb` file's already-in-memory bytes into a
/// [`LoadedMesh`] — for callers that have the bytes in hand (e.g. a Part
/// Pack entry) rather than a filesystem path. `bytes` may be either a full
/// `.glb` file or a `.gltf` JSON document; both are accepted by
/// `gltf::Gltf::from_slice` exactly as `gltf::Gltf::open` accepts either
/// on disk. Uses a synthetic path (`"<embedded bytes>"`) purely for error
/// labeling and `load_buffers`'s `base_dir` computation — the latter is
/// only reached for a relative-file external buffer reference, which a
/// well-formed Part Pack entry never contains (see the Part Pack format
/// doc); if it ever happened anyway, `load_buffers` still returns a clean
/// `Err` rather than panicking.
pub fn load_gltf_bytes(bytes: &[u8]) -> Result<LoadedMesh, GltfLoadError> {
    let synthetic_path = Path::new("<embedded bytes>");
    let gltf = gltf::Gltf::from_slice(bytes).map_err(|source| GltfLoadError::Parse {
        path: synthetic_path.to_path_buf(),
        source,
    })?;
    build_loaded_mesh(gltf.document, gltf.blob, synthetic_path)
}

/// Everything `load_gltf_file`/`load_gltf_bytes` do after successfully
/// obtaining a parsed `gltf::Document` (plus its optional GLB `BIN` blob):
/// resolving buffers, finding the first mesh and its skin, and extracting
/// per-primitive vertex/index data. `path` continues to serve the same
/// role it always has here — error labeling, and `load_buffers`'s
/// `base_dir` computation via `path.parent()` — whether it's a real
/// on-disk path or a synthetic placeholder for an in-memory caller.
fn build_loaded_mesh(
    document: gltf::Document,
    glb_blob: Option<Vec<u8>>,
    path: &Path,
) -> Result<LoadedMesh, GltfLoadError> {
    let buffers = load_buffers(path, &document, glb_blob.as_deref())?;

    let mesh = document.meshes().next().ok_or_else(|| GltfLoadError::NoMeshes {
        path: path.to_path_buf(),
    })?;
    let mesh_index = mesh.index();

    // Locate the node that both references this mesh and carries a skin.
    // A part file may legitimately have several nodes; we need the one
    // that actually drives the mesh we're loading.
    let skin = document
        .nodes()
        .find(|node| {
            node.mesh().map(|m| m.index()) == Some(mesh_index) && node.skin().is_some()
        })
        .and_then(|node| node.skin())
        .ok_or_else(|| GltfLoadError::NoSkin {
            path: path.to_path_buf(),
            mesh_index,
        })?;

    let mut local_bone_names = Vec::new();
    // `node_index -> local_joint_index`, built in one pass over
    // `skin.joints()` so the parent-resolution pass below doesn't need an
    // O(n^2) scan to find which of a joint's children (if any) are
    // themselves members of this skin's joint list.
    let mut node_index_to_local_joint: HashMap<usize, u32> = HashMap::new();
    let mut joint_nodes: Vec<gltf::scene::Node> = Vec::new();
    for (joint_index, joint_node) in skin.joints().enumerate() {
        let name = joint_node.name().ok_or_else(|| GltfLoadError::UnnamedJoint {
            path: path.to_path_buf(),
            joint_index,
        })?;
        local_bone_names.push(name.to_string());
        node_index_to_local_joint.insert(joint_node.index(), joint_index as u32);
        joint_nodes.push(joint_node);
    }

    // Second pass: for each joint, walk its children and, for any child
    // that is itself a member of this skin's joint list, record *this*
    // joint as *that* child's parent (the inverse of the relationship
    // being iterated). A joint with no other joint in the list listing it
    // as a child keeps its default `None` (it's either the skeleton's
    // real root, or this part's skin starts partway down a chain whose
    // upper joints live in a different part — indistinguishable from this
    // part's data alone).
    let mut local_joint_parents: Vec<Option<u32>> = vec![None; joint_nodes.len()];
    for (parent_local_index, joint_node) in joint_nodes.iter().enumerate() {
        for child in joint_node.children() {
            if let Some(&child_local_index) = node_index_to_local_joint.get(&child.index()) {
                local_joint_parents[child_local_index as usize] = Some(parent_local_index as u32);
            }
        }
    }

    let local_joint_bind_poses: Vec<JointBindPose> = joint_nodes
        .iter()
        .map(|joint_node| match joint_node.transform() {
            gltf::scene::Transform::Decomposed {
                translation,
                rotation,
                scale,
            } => JointBindPose {
                translation,
                rotation,
                scale,
            },
            gltf::scene::Transform::Matrix { matrix } => decompose_trs_matrix(matrix),
        })
        .collect();

    let mut vertices: Vec<SkinnedVertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    for primitive in mesh.primitives() {
        let primitive_index = primitive.index();

        if primitive.mode() != gltf::mesh::Mode::Triangles {
            return Err(GltfLoadError::UnsupportedTopology {
                path: path.to_path_buf(),
                mesh_index,
                primitive_index,
                mode: primitive.mode(),
            });
        }

        let reader = primitive.reader(|buffer| Some(&buffers[buffer.index()]));

        let positions: Vec<[f32; 3]> = reader
            .read_positions()
            .ok_or_else(|| GltfLoadError::MissingAccessor {
                path: path.to_path_buf(),
                mesh_index,
                primitive_index,
                accessor: "POSITION",
            })?
            .collect();
        let vertex_count = positions.len();

        let normals: Vec<[f32; 3]> = reader
            .read_normals()
            .ok_or_else(|| GltfLoadError::MissingAccessor {
                path: path.to_path_buf(),
                mesh_index,
                primitive_index,
                accessor: "NORMAL",
            })?
            .collect();
        require_len(
            path,
            mesh_index,
            primitive_index,
            "NORMAL",
            vertex_count,
            normals.len(),
        )?;

        let uvs: Vec<[f32; 2]> = reader
            .read_tex_coords(0)
            .ok_or_else(|| GltfLoadError::MissingAccessor {
                path: path.to_path_buf(),
                mesh_index,
                primitive_index,
                accessor: "TEXCOORD_0",
            })?
            .into_f32()
            .collect();
        require_len(
            path,
            mesh_index,
            primitive_index,
            "TEXCOORD_0",
            vertex_count,
            uvs.len(),
        )?;

        let joints: Vec<[u16; 4]> = reader
            .read_joints(0)
            .ok_or_else(|| GltfLoadError::MissingAccessor {
                path: path.to_path_buf(),
                mesh_index,
                primitive_index,
                accessor: "JOINTS_0",
            })?
            .into_u16()
            .collect();
        require_len(
            path,
            mesh_index,
            primitive_index,
            "JOINTS_0",
            vertex_count,
            joints.len(),
        )?;

        let weights: Vec<[f32; 4]> = reader
            .read_weights(0)
            .ok_or_else(|| GltfLoadError::MissingAccessor {
                path: path.to_path_buf(),
                mesh_index,
                primitive_index,
                accessor: "WEIGHTS_0",
            })?
            .into_f32()
            .collect();
        require_len(
            path,
            mesh_index,
            primitive_index,
            "WEIGHTS_0",
            vertex_count,
            weights.len(),
        )?;

        let primitive_indices: Vec<u32> = reader
            .read_indices()
            .ok_or_else(|| GltfLoadError::MissingIndices {
                path: path.to_path_buf(),
                mesh_index,
                primitive_index,
            })?
            .into_u32()
            .collect();

        // Validate every index against *this primitive's own* vertex count
        // — before offsetting by `base_vertex` or extending the combined
        // `indices` buffer — so a corrupt/malicious index accessor becomes
        // a clean `Err` here rather than an out-of-bounds `indices` entry
        // silently reaching the renderer on the other side of the FFI
        // boundary (the C++ plugin indexes straight into a `TArray` sized
        // to the vertex count with no bounds check in a Shipping build).
        // `obj_loader::load_obj_file` already does this same check for the
        // `.obj` path; this mirrors it for `.gltf`/`.glb`.
        for &index in &primitive_indices {
            if index as usize >= vertex_count {
                return Err(GltfLoadError::IndexOutOfRange {
                    path: path.to_path_buf(),
                    mesh_index,
                    primitive_index,
                    index,
                    vertex_count,
                });
            }
        }

        // Base offset so this primitive's indices point at the right place
        // in the combined `vertices` buffer.
        let base_vertex = vertices.len() as u32;

        for i in 0..vertex_count {
            vertices.push(SkinnedVertex {
                position: positions[i],
                normal: normals[i],
                uv: uvs[i],
                bone_indices: joints[i],
                bone_weights: weights[i],
            });
        }

        indices.extend(primitive_indices.into_iter().map(|idx| idx + base_vertex));
    }

    Ok(LoadedMesh {
        vertices,
        indices,
        local_bone_names,
        local_joint_parents,
        local_joint_bind_poses,
    })
}

/// Resolve every `document.buffers()` entry to its raw bytes.
///
/// Deliberately reimplements (a minimal, geometry-only subset of) what
/// `gltf::import` would otherwise do, so this crate can avoid pulling in
/// the `import` feature's image-decoding dependency chain (see the
/// comment in `Cargo.toml`). Supports the three sources the glTF 2.0 spec
/// defines: the embedded GLB `BIN` chunk, `data:` URIs, and external files
/// referenced by relative path.
fn load_buffers(
    path: &Path,
    document: &gltf::Document,
    glb_blob: Option<&[u8]>,
) -> Result<Vec<Vec<u8>>, GltfLoadError> {
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));

    let mut buffers = Vec::with_capacity(document.buffers().len());

    for buffer in document.buffers() {
        let buffer_index = buffer.index();
        let declared_len = buffer.length();

        let data: Vec<u8> = match buffer.source() {
            gltf::buffer::Source::Bin => glb_blob
                .ok_or_else(|| GltfLoadError::BufferSource {
                    path: path.to_path_buf(),
                    buffer_index,
                    reason: "buffer has no URI (expects the GLB BIN chunk), but this file has no embedded binary chunk".to_string(),
                })?
                .to_vec(),
            gltf::buffer::Source::Uri(uri) => {
                if let Some(encoded) = uri.strip_prefix("data:") {
                    let (_mime, payload) = encoded.split_once(';').ok_or_else(|| {
                        GltfLoadError::BufferSource {
                            path: path.to_path_buf(),
                            buffer_index,
                            reason: format!("malformed data URI (no ';' separator): '{uri}'"),
                        }
                    })?;
                    let payload = payload.strip_prefix("base64,").ok_or_else(|| {
                        GltfLoadError::BufferSource {
                            path: path.to_path_buf(),
                            buffer_index,
                            reason: "data URI is not base64-encoded (only base64 data URIs are supported)".to_string(),
                        }
                    })?;
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload)
                        .map_err(|e| GltfLoadError::BufferSource {
                            path: path.to_path_buf(),
                            buffer_index,
                            reason: format!("invalid base64 in data URI: {e}"),
                        })?
                } else if uri.contains("://") {
                    return Err(GltfLoadError::BufferSource {
                        path: path.to_path_buf(),
                        buffer_index,
                        reason: format!(
                            "unsupported remote URI scheme (only relative file paths and base64 data URIs are supported): '{uri}'"
                        ),
                    });
                } else {
                    let relative = percent_decode(uri);
                    let buffer_path = base_dir.join(relative);
                    std::fs::read(&buffer_path).map_err(|e| GltfLoadError::BufferSource {
                        path: path.to_path_buf(),
                        buffer_index,
                        reason: format!("failed to read external buffer '{}': {e}", buffer_path.display()),
                    })?
                }
            }
        };

        if data.len() < declared_len {
            return Err(GltfLoadError::BufferTooShort {
                path: path.to_path_buf(),
                buffer_index,
                declared_len,
                actual_len: data.len(),
            });
        }

        buffers.push(data);
    }

    Ok(buffers)
}

/// Minimal percent-decoder for relative buffer/image URIs (e.g. spaces
/// encoded as `%20`). Anything that isn't a well-formed `%XX` escape is
/// passed through unchanged rather than treated as an error, since a
/// malformed escape in a filename is still a valid (if unusual) filename.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            let parsed = hex.and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = parsed {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn require_len(
    path: &Path,
    mesh_index: usize,
    primitive_index: usize,
    attribute: &'static str,
    expected: usize,
    found: usize,
) -> Result<(), GltfLoadError> {
    if expected != found {
        return Err(GltfLoadError::AttributeLengthMismatch {
            path: path.to_path_buf(),
            mesh_index,
            primitive_index,
            attribute,
            expected,
            found,
        });
    }
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal, otherwise-well-formed single-triangle `.glb` in
    /// memory: 3 vertices (POSITION/NORMAL/TEXCOORD_0/JOINTS_0/WEIGHTS_0),
    /// one skinned triangle primitive, one named joint. `indices` is the
    /// SCALAR u16 index accessor's raw content, deliberately left as a
    /// parameter so a test can supply either valid (`[0,1,2]`) or
    /// out-of-range (`[0,1,99]`) values against this 3-vertex mesh.
    fn build_test_glb(indices: &[u16]) -> Vec<u8> {
        // Three arbitrary, finite, well-formed triangle vertices.
        let positions: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals: [[f32; 3]; 3] = [[0.0, 0.0, 1.0]; 3];
        let texcoords: [[f32; 2]; 3] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        let joints: [[u8; 4]; 3] = [[0, 0, 0, 0]; 3];
        let weights: [[f32; 4]; 3] = [[1.0, 0.0, 0.0, 0.0]; 3];

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
        for &i in indices {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        let indices_byte_len = indices.len() * 2;
        let buffer_byte_len = bin.len();

        // Pad the BIN chunk to a multiple of 4 bytes per the GLB spec.
        while bin.len() % 4 != 0 {
            bin.push(0);
        }

        let json = serde_json::json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [
                { "mesh": 0, "skin": 0 },
                { "name": "joint0" }
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

    /// Writes `bytes` to a fresh temp file with the given extension and
    /// returns its path; the caller is responsible for removing it.
    fn write_temp_glb(bytes: &[u8], unique_name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("anthroforge_test_{unique_name}.glb"));
        std::fs::write(&path, bytes).expect("write temp glb");
        path
    }

    // ------------------------------------------------------------------
    // `decompose_trs_matrix` — hand-computed input/output pairs.
    // ------------------------------------------------------------------

    fn assert_pose_close(actual: JointBindPose, expected: JointBindPose, tol: f32) {
        for i in 0..3 {
            assert!(
                (actual.translation[i] - expected.translation[i]).abs() < tol,
                "translation[{i}]: expected {}, got {}",
                expected.translation[i],
                actual.translation[i]
            );
            assert!(
                (actual.scale[i] - expected.scale[i]).abs() < tol,
                "scale[{i}]: expected {}, got {}",
                expected.scale[i],
                actual.scale[i]
            );
        }
        for i in 0..4 {
            assert!(
                (actual.rotation[i] - expected.rotation[i]).abs() < tol,
                "rotation[{i}]: expected {}, got {}",
                expected.rotation[i],
                actual.rotation[i]
            );
        }
    }

    #[test]
    fn decompose_identity_matrix() {
        let identity = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let pose = decompose_trs_matrix(identity);
        assert_pose_close(
            pose,
            JointBindPose {
                translation: [0.0, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0, 1.0, 1.0],
            },
            1e-6,
        );
    }

    #[test]
    fn decompose_pure_translation_matrix() {
        // Identity rotation/scale, translation (5, -2, 10).
        let matrix = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [5.0, -2.0, 10.0, 1.0],
        ];
        let pose = decompose_trs_matrix(matrix);
        assert_pose_close(
            pose,
            JointBindPose {
                translation: [5.0, -2.0, 10.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0, 1.0, 1.0],
            },
            1e-6,
        );
    }

    #[test]
    fn decompose_rotation_and_non_uniform_scale_matrix() {
        // A known 90-degree rotation about +Z (row-major R = [[0,-1,0],
        // [1,0,0],[0,0,1]]), combined with non-uniform scale (2, 3, 1)
        // and translation (1, 2, 3). Column-major storage: column i is
        // R's column i scaled by scale[i].
        //
        // R's columns (row-major R above): col0 = (0,1,0), col1 =
        // (-1,0,0), col2 = (0,0,1). Scaled: col0*2 = (0,2,0), col1*3 =
        // (-3,0,0), col2*1 = (0,0,1).
        //
        // Hand-computed expected quaternion for a 90-degree rotation
        // about +Z: (x, y, z, w) = (0, 0, sin(45 deg), cos(45 deg)) =
        // (0, 0, 0.70710678, 0.70710678).
        let matrix = [
            [0.0, 2.0, 0.0, 0.0],
            [-3.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [1.0, 2.0, 3.0, 1.0],
        ];
        let pose = decompose_trs_matrix(matrix);
        assert_pose_close(
            pose,
            JointBindPose {
                translation: [1.0, 2.0, 3.0],
                rotation: [0.0, 0.0, 0.70710678, 0.70710678],
                scale: [2.0, 3.0, 1.0],
            },
            1e-4,
        );
    }

    /// Builds a small synthetic skinned `.glb` with a real 3-joint chain
    /// (`root` -> `spine` -> `head`), deliberately non-trivial per the
    /// Phase 4 skeleton-extraction task write-up: `spine` has a
    /// non-identity (decomposed-TRS) translation, and `head`'s bind pose
    /// is authored as a raw `Matrix` combining a non-identity rotation
    /// with non-uniform scale (exercising `decompose_trs_matrix` through
    /// the real node-hierarchy-walking code path, not just in isolation).
    /// Node 0 drives the (trivial, single-triangle) mesh/skin; nodes 1-3
    /// are the joints, listed in the skin's `joints` array as
    /// `[1, 2, 3]` (local indices 0, 1, 2 = root, spine, head).
    fn build_hierarchy_test_glb() -> Vec<u8> {
        let positions: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals: [[f32; 3]; 3] = [[0.0, 0.0, 1.0]; 3];
        let texcoords: [[f32; 2]; 3] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        // All fully weighted onto local joint 0 ("root") — vertex skinning
        // weights are irrelevant to this test, only the joint hierarchy/
        // bind-pose extraction is under test here.
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

        // head's bind pose, authored as a raw Matrix: 90-degree rotation
        // about +Z, non-uniform scale (2, 3, 1), translation (0, 0, 2.5).
        // Column-major, flattened.
        let head_matrix: [f32; 16] = [
            0.0, 2.0, 0.0, 0.0, // column 0
            -3.0, 0.0, 0.0, 0.0, // column 1
            0.0, 0.0, 1.0, 0.0, // column 2
            0.0, 0.0, 2.5, 1.0, // column 3 (translation)
        ];

        let json = serde_json::json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [
                { "mesh": 0, "skin": 0 },
                { "name": "root", "children": [2] },
                { "name": "spine", "translation": [0.0, 1.5, 0.0], "children": [3] },
                { "name": "head", "matrix": head_matrix }
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
            "skins": [{ "joints": [1, 2, 3] }],
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

    /// This is the "new, separate, deliberately non-trivial" fixture the
    /// Phase 4 task write-up requires: `tests/fixtures/real_pack_e2e/`'s
    /// two `.glb` files each define exactly one identity-transform,
    /// parentless `"root"` joint, which would pass even a completely
    /// broken hierarchy-builder or matrix-decomposition function by doing
    /// nothing. This fixture has a real 3-joint parent chain and
    /// non-identity transforms (one decomposed-TRS translation, one raw
    /// Matrix combining rotation + non-uniform scale), so a bug in either
    /// actually changes this test's outcome.
    #[test]
    fn hierarchy_glb_extracts_real_parents_and_bind_poses() {
        let glb = build_hierarchy_test_glb();
        let path = write_temp_glb(&glb, "hierarchy_chain");

        let result = load_gltf_file(&path);
        std::fs::remove_file(&path).ok();

        let loaded = result.expect("well-formed hierarchy glb must load");

        assert_eq!(loaded.local_bone_names, vec!["root", "spine", "head"]);
        assert_eq!(
            loaded.local_joint_parents,
            vec![None, Some(0), Some(1)],
            "root has no parent; spine's parent is root (local 0); head's parent is spine (local 1)"
        );
        assert_eq!(loaded.local_joint_bind_poses.len(), 3);

        // root: untouched, default-identity glTF node transform.
        assert_pose_close(
            loaded.local_joint_bind_poses[0],
            JointBindPose {
                translation: [0.0, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0, 1.0, 1.0],
            },
            1e-4,
        );

        // spine: decomposed-TRS translation only.
        assert_pose_close(
            loaded.local_joint_bind_poses[1],
            JointBindPose {
                translation: [0.0, 1.5, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0, 1.0, 1.0],
            },
            1e-4,
        );

        // head: raw Matrix, must come back decomposed correctly.
        assert_pose_close(
            loaded.local_joint_bind_poses[2],
            JointBindPose {
                translation: [0.0, 0.0, 2.5],
                rotation: [0.0, 0.0, 0.70710678, 0.70710678],
                scale: [2.0, 3.0, 1.0],
            },
            1e-4,
        );
    }

    #[test]
    fn well_formed_glb_with_valid_indices_loads_successfully() {
        let glb = build_test_glb(&[0, 1, 2]);
        let path = write_temp_glb(&glb, "valid_indices");

        let result = load_gltf_file(&path);
        std::fs::remove_file(&path).ok();

        let loaded = result.expect("well-formed glb with valid indices must load");
        assert_eq!(loaded.vertices.len(), 3);
        assert_eq!(loaded.indices, vec![0, 1, 2]);
    }

    /// Regression test for the out-of-bounds-index gap this audit found:
    /// a `.glb` whose index accessor references a vertex past the end of
    /// the primitive's own attribute arrays must be rejected with
    /// `IndexOutOfRange`, not silently produce an `indices` buffer that
    /// would read out of bounds on the C++/FFI side of the mesh consumer.
    #[test]
    fn malformed_glb_with_out_of_range_index_is_rejected() {
        // 3 vertices (indices 0..=2 valid), but the index accessor
        // references vertex 99, which does not exist.
        let glb = build_test_glb(&[0, 1, 99]);
        let path = write_temp_glb(&glb, "out_of_range_index");

        let result = load_gltf_file(&path);
        std::fs::remove_file(&path).ok();

        let err = match result {
            Ok(_) => panic!("expected IndexOutOfRange, got Ok(..)"),
            Err(e) => e,
        };
        match err {
            GltfLoadError::IndexOutOfRange { index, vertex_count, .. } => {
                assert_eq!(index, 99);
                assert_eq!(vertex_count, 3);
            }
            other => panic!("expected IndexOutOfRange, got {other:?}"),
        }
    }
}
