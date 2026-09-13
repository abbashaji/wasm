//! `mesh_merge` — standalone multi-part mesh merging utility.
//!
//! `anthroforge-core` loads modular parts (heads, torsos, clothing items)
//! as separate vertex/index buffers, but has no utility for combining
//! several of them into a single draw-ready buffer. This module implements
//! that as a pure, standalone function: [`merge_parts`] concatenates each
//! part's vertex buffer, in order, and rebases each part's index buffer by
//! that part's cumulative vertex offset so the combined index buffer
//! correctly indexes into the combined vertex buffer.
//!
//! This is intentionally *not* wired into `generate_character` or any
//! registry logic here — that integration happens elsewhere. This module
//! is not declared in `lib.rs` yet either; it is implemented and tested in
//! isolation.
//!
//! Explicitly out of scope (see the task write-up):
//! - No deduplication/welding of shared or seam vertices between parts.
//!   Each part's vertices are kept exactly as given.
//! - No validation of index values *within* a part being in-range for
//!   that part's own vertex buffer — callers (this crate's loaders) are
//!   assumed to have already validated that on load.

use crate::SkinnedVertex;

/// Errors [`merge_parts`] can return. No panics — every failure mode is a
/// typed variant instead.
#[derive(Debug)]
pub enum MeshMergeError {
    /// A part's index count was not a multiple of 3. This crate's loaders
    /// always emit triangle lists (see `gltf_loader.rs`/`obj_loader.rs`),
    /// so an index count that isn't a multiple of 3 means the part's
    /// buffers are malformed.
    IndexCountNotMultipleOfThree {
        part_index: usize,
        index_count: usize,
    },
    /// The cumulative vertex offset (the running sum of vertex counts of
    /// every part before this one) would exceed `u32::MAX` once this
    /// part's own vertex count is folded in, which would make some of
    /// this part's rebased indices unrepresentable as `u32`. Returned
    /// instead of panicking (debug builds) or silently wrapping (release
    /// builds).
    VertexOffsetOverflow { part_index: usize },
}

impl std::fmt::Display for MeshMergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeshMergeError::IndexCountNotMultipleOfThree {
                part_index,
                index_count,
            } => write!(
                f,
                "part[{part_index}] has {index_count} indices, which is not a multiple of 3 \
                 (this crate's loaders only ever emit triangle lists)"
            ),
            MeshMergeError::VertexOffsetOverflow { part_index } => write!(
                f,
                "cumulative vertex offset overflowed u32::MAX before part[{part_index}]; \
                 too many/too-large parts to merge into a single u32-indexed buffer"
            ),
        }
    }
}

impl std::error::Error for MeshMergeError {}

/// Concatenate several modular parts' vertex/index buffers into a single
/// draw-ready buffer.
///
/// Each entry in `parts` is `(vertices, indices)` for one part. Vertex
/// buffers are concatenated in the order given. Each part's index buffer
/// is rebased by that part's cumulative vertex offset (the sum of vertex
/// counts of every part before it) before being appended to the combined
/// index buffer, so e.g. part 2's index `0` becomes `part_1.vertices.len()`
/// in the combined buffer.
///
/// An empty `parts` slice is not an error — it returns `Ok((vec![], vec![]))`.
/// Whether zero parts is a meaningful failure is the caller's business
/// logic, not this utility's concern.
pub fn merge_parts(
    parts: &[(&[SkinnedVertex], &[u32])],
) -> Result<(Vec<SkinnedVertex>, Vec<u32>), MeshMergeError> {
    let mut combined_vertices: Vec<SkinnedVertex> = Vec::new();
    let mut combined_indices: Vec<u32> = Vec::new();

    // Cumulative vertex offset for the part about to be processed: the sum
    // of vertex counts of every part that came before it.
    let mut cumulative_offset: u32 = 0;

    for (part_index, (vertices, indices)) in parts.iter().enumerate() {
        if indices.len() % 3 != 0 {
            return Err(MeshMergeError::IndexCountNotMultipleOfThree {
                part_index,
                index_count: indices.len(),
            });
        }

        combined_vertices.extend_from_slice(vertices);
        combined_indices.extend(indices.iter().map(|&index| index + cumulative_offset));

        let vertex_count: u32 = match u32::try_from(vertices.len()) {
            Ok(count) => count,
            Err(_) => return Err(MeshMergeError::VertexOffsetOverflow { part_index }),
        };

        cumulative_offset = match cumulative_offset.checked_add(vertex_count) {
            Some(sum) => sum,
            None => return Err(MeshMergeError::VertexOffsetOverflow { part_index }),
        };
    }

    Ok((combined_vertices, combined_indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vertex(x: f32) -> SkinnedVertex {
        SkinnedVertex {
            position: [x, 0.0, 0.0],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            bone_indices: [0, 0, 0, 0],
            bone_weights: [1.0, 0.0, 0.0, 0.0],
        }
    }

    #[test]
    fn merging_two_triangles_offsets_second_by_first_vertex_count() {
        let part_a_vertices = [make_vertex(0.0), make_vertex(1.0), make_vertex(2.0)];
        let part_a_indices: [u32; 3] = [0, 1, 2];

        let part_b_vertices = [make_vertex(3.0), make_vertex(4.0), make_vertex(5.0)];
        let part_b_indices: [u32; 3] = [0, 1, 2];

        let parts: [(&[SkinnedVertex], &[u32]); 2] = [
            (&part_a_vertices, &part_a_indices),
            (&part_b_vertices, &part_b_indices),
        ];

        let (vertices, indices) = merge_parts(&parts).expect("merge should succeed");

        assert_eq!(vertices.len(), 6);
        assert_eq!(indices.len(), 6);
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5]);
        // Sanity-check the vertices themselves were concatenated in order.
        assert_eq!(vertices[0].position[0], 0.0);
        assert_eq!(vertices[3].position[0], 3.0);
    }

    #[test]
    fn merging_single_part_is_identity() {
        let vertices = [make_vertex(0.0), make_vertex(1.0), make_vertex(2.0)];
        let indices: [u32; 3] = [0, 1, 2];

        let parts: [(&[SkinnedVertex], &[u32]); 1] = [(&vertices, &indices)];

        let (merged_vertices, merged_indices) = merge_parts(&parts).expect("merge should succeed");

        assert_eq!(merged_vertices.len(), vertices.len());
        assert_eq!(merged_indices, indices.to_vec());
        for (merged, original) in merged_vertices.iter().zip(vertices.iter()) {
            assert_eq!(merged.position, original.position);
        }
    }

    #[test]
    fn empty_parts_slice_is_not_an_error() {
        let parts: [(&[SkinnedVertex], &[u32]); 0] = [];
        let (vertices, indices) = merge_parts(&parts).expect("empty input must be Ok");
        assert!(vertices.is_empty());
        assert!(indices.is_empty());
    }

    #[test]
    fn non_multiple_of_three_index_count_is_a_typed_error() {
        let vertices = [make_vertex(0.0), make_vertex(1.0), make_vertex(2.0), make_vertex(3.0)];
        // 4 indices: not a multiple of 3.
        let bad_indices: [u32; 4] = [0, 1, 2, 3];

        let parts: [(&[SkinnedVertex], &[u32]); 1] = [(&vertices, &bad_indices)];

        let result = merge_parts(&parts);
        match result {
            Ok(_) => panic!("expected IndexCountNotMultipleOfThree, got Ok"),
            Err(MeshMergeError::IndexCountNotMultipleOfThree {
                part_index,
                index_count,
            }) => {
                assert_eq!(part_index, 0);
                assert_eq!(index_count, 4);
            }
            Err(other) => panic!("expected IndexCountNotMultipleOfThree, got {other:?}"),
        }
    }

    #[test]
    fn three_parts_produce_correctly_cumulative_offsets() {
        let part_a_vertices = [make_vertex(0.0), make_vertex(1.0), make_vertex(2.0)];
        let part_a_indices: [u32; 3] = [0, 1, 2];

        // Part B has 4 vertices this time, to make sure the offset isn't
        // just coincidentally right due to every part being the same size.
        let part_b_vertices = [
            make_vertex(10.0),
            make_vertex(11.0),
            make_vertex(12.0),
            make_vertex(13.0),
        ];
        let part_b_indices: [u32; 3] = [0, 1, 2];

        let part_c_vertices = [make_vertex(20.0), make_vertex(21.0), make_vertex(22.0)];
        let part_c_indices: [u32; 3] = [2, 1, 0];

        let parts: [(&[SkinnedVertex], &[u32]); 3] = [
            (&part_a_vertices, &part_a_indices),
            (&part_b_vertices, &part_b_indices),
            (&part_c_vertices, &part_c_indices),
        ];

        let (vertices, indices) = merge_parts(&parts).expect("merge should succeed");

        assert_eq!(vertices.len(), 3 + 4 + 3);
        // Part A: offset 0.
        assert_eq!(&indices[0..3], &[0, 1, 2]);
        // Part B: offset 3 (part A's vertex count).
        assert_eq!(&indices[3..6], &[3, 4, 5]);
        // Part C: offset 3 + 4 = 7 (sum of part A's and part B's vertex counts).
        assert_eq!(&indices[6..9], &[9, 8, 7]);
    }
}
