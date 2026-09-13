//! `skeleton_resolver` — the Master Skeleton Alignment Matrix.
//!
//! Every modular part is authored against its own local skin (its
//! `JOINTS_0` accessor indexes into that part's own joint list). If two
//! parts from different source files were combined without correction,
//! "local joint 3" in the head might mean `neck_01` while "local joint 3"
//! in the torso means `spine_02` — same index, different bone, which tears
//! the mesh apart at runtime.
//!
//! This module loads `master_skeleton.json` (a flat bone-name -> global
//! index map) once, then rewrites each part's per-vertex `bone_indices`
//! from part-local joint indices to that single global numbering, using
//! bone *names* as the join key. A part that references a bone name absent
//! from the master skeleton is a data error, not something to silently
//! paper over, so it is rejected with `Err`.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::gltf_loader::JointBindPose;
use crate::SkinnedVertex;

/// The name -> global-index lookup table (loaded verbatim from
/// `master_skeleton.json`, which stays a simple flat `{ "bone_name":
/// index, ... }` object — see [`load_master_skeleton`]/
/// [`parse_master_skeleton_bytes`]) *plus* the growing, authoritative
/// global skeleton this module assembles across every loaded part (see
/// [`contribute_global_joints`]).
pub struct MasterSkeleton {
    pub name_to_global_index: HashMap<String, u32>,
    /// Indexed by global bone index. `None` until some part has
    /// contributed data for that index; must be fully `Some` for every
    /// index by the time all of a registry's parts have been loaded (see
    /// `lib.rs`'s completeness check), or that whole init call fails.
    pub global_joints: Vec<Option<GlobalJoint>>,
}

/// One bone's resolved position in the assembled global skeleton: its
/// parent (by global index, or `None` if it IS the root) and its
/// bind-pose local transform.
#[derive(Clone, Copy, Debug)]
pub struct GlobalJoint {
    pub parent: Option<u32>,
    pub bind_pose: JointBindPose,
}

#[derive(Debug)]
pub enum SkeletonError {
    /// `master_skeleton.json` could not be read from disk.
    Io { path: PathBuf, source: std::io::Error },
    /// `master_skeleton.json` was not valid JSON, or not shaped as a flat
    /// `{ "bone_name": index, ... }` object.
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// A part referenced a bone name that does not exist anywhere in the
    /// master skeleton. This is intentionally NOT silently defaulted, per
    /// spec: a missing bone means the part is incompatible with the rig
    /// and must be rejected outright. Also reused by
    /// [`contribute_global_joints`] for the equivalent failure mode: a
    /// joint's *parent* name not existing in the master skeleton is the
    /// same underlying data error as the joint's own name not existing.
    UnknownBone {
        bone_name: String,
        local_joint_index: usize,
    },
    /// A vertex's local `bone_indices` entry pointed past the end of the
    /// part's own joint list — malformed/corrupt part data.
    LocalJointIndexOutOfRange {
        vertex_index: usize,
        local_index: u16,
        local_joint_count: usize,
    },
    /// Two different parts both contributed data for the same global
    /// bone, but disagree about its parent. Two parts disagreeing about a
    /// shared skeleton's structure is a real authoring bug worth
    /// surfacing loudly, not something to silently paper over.
    ParentMismatch {
        bone_name: String,
        existing_parent: Option<String>,
        new_parent: Option<String>,
    },
    /// Two different parts both contributed data for the same global
    /// bone, and their parents agree, but their bind-pose transforms
    /// disagree by more than the allowed floating-point tolerance.
    BindPoseMismatch {
        bone_name: String,
        existing: JointBindPose,
        new: JointBindPose,
    },
}

impl fmt::Display for SkeletonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkeletonError::Io { path, source } => {
                write!(f, "failed to read '{}': {source}", path.display())
            }
            SkeletonError::Parse { path, source } => {
                write!(f, "failed to parse '{}' as JSON: {source}", path.display())
            }
            SkeletonError::UnknownBone {
                bone_name,
                local_joint_index,
            } => write!(
                f,
                "bone '{bone_name}' (local joint index {local_joint_index}) does not exist in master_skeleton.json; part is incompatible with the master rig"
            ),
            SkeletonError::LocalJointIndexOutOfRange {
                vertex_index,
                local_index,
                local_joint_count,
            } => write!(
                f,
                "vertex[{vertex_index}] references local joint index {local_index}, but the part only defines {local_joint_count} joints"
            ),
            SkeletonError::ParentMismatch {
                bone_name,
                existing_parent,
                new_parent,
            } => {
                let fmt_parent = |p: &Option<String>| match p {
                    Some(name) => name.clone(),
                    None => "<none, i.e. this part claims it's the root>".to_string(),
                };
                write!(
                    f,
                    "bone '{bone_name}' was already contributed by an earlier part with parent {}, but this part claims its parent is {} — two parts disagree about this shared skeleton's structure",
                    fmt_parent(existing_parent),
                    fmt_parent(new_parent)
                )
            }
            SkeletonError::BindPoseMismatch {
                bone_name,
                existing,
                new,
            } => write!(
                f,
                "bone '{bone_name}' was already contributed by an earlier part with bind pose translation={:?} rotation={:?} scale={:?}, but this part claims translation={:?} rotation={:?} scale={:?} — differs by more than the allowed floating-point tolerance",
                existing.translation, existing.rotation, existing.scale,
                new.translation, new.rotation, new.scale,
            ),
        }
    }
}

impl std::error::Error for SkeletonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SkeletonError::Io { source, .. } => Some(source),
            SkeletonError::Parse { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Load `master_skeleton.json` (a flat `{ "bone_name": global_index, ... }`
/// object) from disk. Never panics; I/O and parse failures both become
/// `Err(SkeletonError)`.
pub fn load_master_skeleton(path: &Path) -> Result<MasterSkeleton, SkeletonError> {
    let raw = std::fs::read_to_string(path).map_err(|source| SkeletonError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    parse_master_skeleton_bytes(raw.as_bytes(), path)
}

/// Same JSON parsing/validation as [`load_master_skeleton`], but from an
/// already-in-memory buffer instead of reading a file — for targets with
/// no filesystem (`wasm32-unknown-unknown`; see the AnthroForge/Web spec
/// §4.2). `path_for_errors` labels any `SkeletonError::Parse` diagnostic;
/// for a bytes-only caller this is a synthetic placeholder, never used
/// for I/O here.
pub fn parse_master_skeleton_bytes(
    bytes: &[u8],
    path_for_errors: &Path,
) -> Result<MasterSkeleton, SkeletonError> {
    // The on-disk/in-pack format is unchanged by this module's expanded
    // in-memory representation: still a flat `{ "bone_name": index, ... }`
    // object, nothing more.
    let name_to_global_index: HashMap<String, u32> =
        serde_json::from_slice(bytes).map_err(|source| SkeletonError::Parse {
            path: path_for_errors.to_path_buf(),
            source,
        })?;

    let global_joints = vec![None; name_to_global_index.len()];

    Ok(MasterSkeleton {
        name_to_global_index,
        global_joints,
    })
}

/// Rewrite `vertices[..].bone_indices` in place from part-local joint
/// indices (indices into `local_bone_names`) to global master-skeleton
/// bone indices.
///
/// `local_bone_names[i]` must be the bone name for part-local joint index
/// `i` (this is exactly what [`crate::gltf_loader::LoadedMesh::local_bone_names`]
/// provides). Every local index actually referenced by a vertex is
/// resolved through `master` by name; a name with no entry in `master` is
/// a hard error (`SkeletonError::UnknownBone`) rather than a silent
/// default, since silently defaulting would reintroduce the exact vertex
/// tearing this module exists to prevent.
pub fn resolve_bone_indices(
    vertices: &mut [SkinnedVertex],
    local_bone_names: &[String],
    master: &MasterSkeleton,
) -> Result<(), SkeletonError> {
    // Resolve the full local -> global lookup table once, up front. This
    // both validates every bone the part uses (regardless of whether a
    // given vertex happens to reference it with nonzero weight) and avoids
    // repeated HashMap lookups per-vertex.
    let mut local_to_global: Vec<u32> = Vec::with_capacity(local_bone_names.len());
    for (local_joint_index, bone_name) in local_bone_names.iter().enumerate() {
        let global_index = *master
            .name_to_global_index
            .get(bone_name)
            .ok_or_else(|| SkeletonError::UnknownBone {
                bone_name: bone_name.clone(),
                local_joint_index,
            })?;
        local_to_global.push(global_index);
    }

    let local_joint_count = local_bone_names.len();

    for (vertex_index, vertex) in vertices.iter_mut().enumerate() {
        for slot in 0..4 {
            let local_index = vertex.bone_indices[slot];

            // A weight of 0 means this influence slot is unused; some
            // exporters leave the accompanying index as a stale/garbage
            // 0 rather than a valid joint reference. Skip remapping slots
            // that carry no influence so they can't spuriously fail the
            // range check below.
            if vertex.bone_weights[slot] == 0.0 {
                continue;
            }

            if local_index as usize >= local_joint_count {
                return Err(SkeletonError::LocalJointIndexOutOfRange {
                    vertex_index,
                    local_index,
                    local_joint_count,
                });
            }

            let global_index = local_to_global[local_index as usize];
            // SkinnedVertex.bone_indices is u16; master skeleton indices
            // are expected to fit comfortably within a full production
            // rig's bone count. Saturate rather than silently truncate/
            // wrap if a pathological master skeleton ever exceeds u16::MAX
            // bones.
            vertex.bone_indices[slot] = global_index.min(u16::MAX as u32) as u16;
        }
    }

    Ok(())
}

/// How close two bind-pose transforms' components must be to be treated
/// as "the same" transform authored twice (once per contributing part).
/// Floating-point parsing/precision differences between two independently
/// authored files describing "the same" transform are expected and not a
/// real inconsistency — this is deliberately *not* exact equality.
const BIND_POSE_TOLERANCE: f32 = 1e-4;

fn bind_poses_match_within_tolerance(a: &JointBindPose, b: &JointBindPose) -> bool {
    let close = |x: f32, y: f32| (x - y).abs() < BIND_POSE_TOLERANCE;
    a.translation
        .iter()
        .zip(b.translation.iter())
        .all(|(&x, &y)| close(x, y))
        && a.rotation
            .iter()
            .zip(b.rotation.iter())
            .all(|(&x, &y)| close(x, y))
        && a.scale.iter().zip(b.scale.iter()).all(|(&x, &y)| close(x, y))
}

/// Looks up the bone name for a global index, for error messages only.
/// Builds a small reverse map on the fly (this only ever runs on an
/// already-erroring path, never in the hot loop) rather than maintaining
/// one permanently on `MasterSkeleton`.
fn bone_name_for_global_index(master: &MasterSkeleton, global_index: u32) -> String {
    master
        .name_to_global_index
        .iter()
        .find(|&(_, &v)| v == global_index)
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| format!("<unnamed bone #{global_index}>"))
}

/// Contributes one loaded part's joint hierarchy + bind poses into the
/// growing, authoritative global skeleton (`master.global_joints`).
/// Called once per loaded glTF/GLB part (see `lib.rs`'s
/// `init_part_registry_impl`/`init_part_registry_from_pack_impl`, right
/// alongside their existing `resolve_bone_indices` call).
///
/// For each local joint `i`: `local_bone_names[i]` and (if present)
/// the bone name of its parent (`local_bone_names[local_joint_parents[i]]`)
/// are both resolved against `master.name_to_global_index` — an
/// unresolvable name, whether the joint's own or its parent's, is the
/// same underlying data error and reuses `SkeletonError::UnknownBone`
/// (the same variant/lookup path `resolve_bone_indices` already uses).
///
/// If this is the first part to contribute data for a given global bone,
/// its parent + bind pose are recorded as-is. If another part already
/// contributed that bone, the two contributions must agree: the parent
/// must match exactly (a mismatch is a hard `Err` naming both parts'
/// claimed parent — two parts disagreeing about a shared skeleton's
/// structure is a real authoring bug worth surfacing loudly), and the
/// bind pose must match within [`BIND_POSE_TOLERANCE`] per component (a
/// mismatch beyond that tolerance is also a hard `Err`, naming the
/// specific joint and both values).
pub fn contribute_global_joints(
    master: &mut MasterSkeleton,
    local_bone_names: &[String],
    local_joint_parents: &[Option<u32>],
    local_joint_bind_poses: &[JointBindPose],
) -> Result<(), SkeletonError> {
    // Resolve every local joint's own global index, and (separately) its
    // parent's global index if it has one, up front — mirroring
    // `resolve_bone_indices`'s existing "resolve the full lookup table
    // before touching any shared state" structure — before writing
    // anything into `master.global_joints`.
    let mut local_to_global: Vec<u32> = Vec::with_capacity(local_bone_names.len());
    for (local_joint_index, bone_name) in local_bone_names.iter().enumerate() {
        let global_index = *master
            .name_to_global_index
            .get(bone_name)
            .ok_or_else(|| SkeletonError::UnknownBone {
                bone_name: bone_name.clone(),
                local_joint_index,
            })?;
        local_to_global.push(global_index);
    }

    let mut resolved_parents: Vec<Option<u32>> = Vec::with_capacity(local_joint_parents.len());
    for (local_joint_index, parent) in local_joint_parents.iter().enumerate() {
        let resolved = match parent {
            None => None,
            Some(parent_local_index) => {
                // `local_joint_parents` is always produced by
                // `gltf_loader::build_loaded_mesh` from indices into this
                // very same array, so an out-of-range entry here would
                // indicate an internal contract violation rather than a
                // real authoring/data error — handled defensively (this
                // crate never panics) by reusing `UnknownBone` rather
                // than indexing directly.
                let parent_name = local_bone_names.get(*parent_local_index as usize).ok_or_else(|| {
                    SkeletonError::UnknownBone {
                        bone_name: format!(
                            "<internal error: parent local joint index {parent_local_index} is out of range for this part's {} joints>",
                            local_bone_names.len()
                        ),
                        local_joint_index,
                    }
                })?;
                let parent_global = *master.name_to_global_index.get(parent_name).ok_or_else(|| {
                    SkeletonError::UnknownBone {
                        bone_name: parent_name.clone(),
                        local_joint_index,
                    }
                })?;
                Some(parent_global)
            }
        };
        resolved_parents.push(resolved);
    }

    for local_joint_index in 0..local_bone_names.len() {
        let global_index = local_to_global[local_joint_index] as usize;
        let parent = resolved_parents[local_joint_index];
        let bind_pose = local_joint_bind_poses[local_joint_index];
        let bone_name = &local_bone_names[local_joint_index];

        match master.global_joints[global_index] {
            None => {
                master.global_joints[global_index] = Some(GlobalJoint { parent, bind_pose });
            }
            Some(existing) => {
                if existing.parent != parent {
                    return Err(SkeletonError::ParentMismatch {
                        bone_name: bone_name.clone(),
                        existing_parent: existing.parent.map(|g| bone_name_for_global_index(master, g)),
                        new_parent: parent.map(|g| bone_name_for_global_index(master, g)),
                    });
                }
                if !bind_poses_match_within_tolerance(&existing.bind_pose, &bind_pose) {
                    return Err(SkeletonError::BindPoseMismatch {
                        bone_name: bone_name.clone(),
                        existing: existing.bind_pose,
                        new: bind_pose,
                    });
                }
            }
        }
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_pose() -> JointBindPose {
        JointBindPose {
            translation: [0.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0, 1.0, 1.0],
        }
    }

    fn test_master(names: &[(&str, u32)]) -> MasterSkeleton {
        let name_to_global_index: HashMap<String, u32> = names
            .iter()
            .map(|(name, idx)| (name.to_string(), *idx))
            .collect();
        let global_joints = vec![None; name_to_global_index.len()];
        MasterSkeleton {
            name_to_global_index,
            global_joints,
        }
    }

    /// Two parts contributing the *same* joint name with matching parent
    /// and (within tolerance) matching bind pose must both succeed, and
    /// the second contribution must not disturb the first's recorded
    /// data.
    #[test]
    fn contribute_global_joints_matching_cross_part_contribution_succeeds() {
        let mut master = test_master(&[("root", 0), ("spine", 1)]);

        // Part A contributes both joints.
        contribute_global_joints(
            &mut master,
            &["root".to_string(), "spine".to_string()],
            &[None, Some(0)],
            &[
                identity_pose(),
                JointBindPose {
                    translation: [0.0, 1.5, 0.0],
                    ..identity_pose()
                },
            ],
        )
        .expect("part A's first contribution must succeed");

        // Part B re-contributes "spine" with a matching parent and a
        // bind pose that differs only by floating-point noise well
        // within tolerance.
        contribute_global_joints(
            &mut master,
            &["root".to_string(), "spine".to_string()],
            &[None, Some(0)],
            &[
                identity_pose(),
                JointBindPose {
                    translation: [0.0, 1.5000005, 0.0],
                    ..identity_pose()
                },
            ],
        )
        .expect("part B's matching re-contribution must succeed");

        let spine = master.global_joints[1].expect("spine must be contributed");
        assert_eq!(spine.parent, Some(0));
        assert!((spine.bind_pose.translation[1] - 1.5).abs() < 1e-3);
    }

    /// Two parts contributing the same joint name but disagreeing about
    /// its parent must fail with a specific, readable error naming the
    /// conflicting joint.
    #[test]
    fn contribute_global_joints_parent_mismatch_is_a_hard_error() {
        let mut master = test_master(&[("root", 0), ("alt_root", 1), ("spine", 2)]);

        contribute_global_joints(
            &mut master,
            &["root".to_string(), "spine".to_string()],
            &[None, Some(0)],
            &[identity_pose(), identity_pose()],
        )
        .expect("part A's contribution must succeed");

        let err = contribute_global_joints(
            &mut master,
            &["alt_root".to_string(), "spine".to_string()],
            &[None, Some(0)],
            &[identity_pose(), identity_pose()],
        )
        .expect_err("part B claiming a different parent for 'spine' must fail");

        match &err {
            SkeletonError::ParentMismatch { bone_name, .. } => {
                assert_eq!(bone_name, "spine");
            }
            other => panic!("expected ParentMismatch, got {other:?}"),
        }
        let message = err_to_string(&err);
        assert!(message.contains("spine"), "error should name the conflicting joint: {message}");
    }

    /// Two parts contributing the same joint name with a matching parent
    /// but a bind pose that disagrees by more than the allowed tolerance
    /// must fail with a specific, readable error naming the conflicting
    /// joint and both values.
    #[test]
    fn contribute_global_joints_bind_pose_mismatch_is_a_hard_error() {
        let mut master = test_master(&[("root", 0)]);

        contribute_global_joints(
            &mut master,
            &["root".to_string()],
            &[None],
            &[identity_pose()],
        )
        .expect("part A's contribution must succeed");

        let err = contribute_global_joints(
            &mut master,
            &["root".to_string()],
            &[None],
            &[JointBindPose {
                translation: [0.0, 10.0, 0.0], // far beyond tolerance
                ..identity_pose()
            }],
        )
        .expect_err("part B claiming a wildly different bind pose for 'root' must fail");

        match err {
            SkeletonError::BindPoseMismatch { bone_name, .. } => {
                assert_eq!(bone_name, "root");
            }
            other => panic!("expected BindPoseMismatch, got {other:?}"),
        }
    }

    /// A bone named in the master skeleton that no loaded part ever
    /// contributes must be detectable after the fact (exercised directly
    /// here at the `MasterSkeleton`/`contribute_global_joints` level;
    /// `lib.rs`'s own tests exercise the same invariant at the
    /// `init_part_registry*` level via its `check_global_joints_complete`
    /// helper).
    #[test]
    fn uncontributed_bone_remains_none() {
        let mut master = test_master(&[("root", 0), ("never_contributed", 1)]);

        contribute_global_joints(
            &mut master,
            &["root".to_string()],
            &[None],
            &[identity_pose()],
        )
        .expect("contributing 'root' must succeed");

        assert!(master.global_joints[0].is_some());
        assert!(
            master.global_joints[1].is_none(),
            "'never_contributed' was never contributed by any part and must remain None"
        );
    }

    fn err_to_string(e: &SkeletonError) -> String {
        format!("{e}")
    }
}
