//! `body_mutation` — DNA-driven body mutation of a modular part's skin.
//!
//! [`mutate_skin_vertices`] applies a non-uniform per-axis scale to a
//! buffer of [`SkinnedVertex`]s: positions are scaled directly, normals
//! are transformed by the inverse-scale-then-renormalize rule that is
//! correct for a diagonal (axis-aligned, non-uniform) scale matrix, and
//! `uv`/`bone_indices`/`bone_weights` pass through unchanged.
//!
//! # Asset-authoring assumption
//! This function scales every position about the origin `(0, 0, 0)`. That
//! is only the character's actual pivot if every modular part is authored
//! sharing a common origin/pivot — specifically, the character's vertical
//! centerline with feet at `y = 0`. This function has no way to verify
//! that a given `vertices` buffer satisfies that assumption; it is a
//! contract the asset pipeline must uphold, not something enforced here.
//!
//! # Scope note
//! This module deliberately does not know about `CharacterDNA` or
//! `seed: u64` — it takes a plain `[f32; 3]` scale and nothing else.
//! Mapping `CharacterDNA` to a scale vector is
//! `clothing_deformer::dna_scale_from_character_dna`'s job, and wiring
//! this function into `generate_character` is a separate integration
//! step not addressed here.

use crate::SkinnedVertex;

/// Every fallible operation in this module returns one of these instead of
/// panicking — required because this module's functions are, directly or
/// via a thin `extern "C"` wrapper, reachable from the FFI boundary, and
/// unwinding across it is undefined behavior.
#[derive(Debug)]
pub enum BodyMutationError {
    /// A `scale` component was `<= 0.0`, `NaN`, or infinite. Non-positive
    /// scale would mirror or degenerate the mesh, so it is rejected
    /// outright rather than clamped or silently corrected.
    InvalidScale { component_index: usize, value: f32 },
}

impl std::fmt::Display for BodyMutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyMutationError::InvalidScale { component_index, value } => write!(
                f,
                "scale[{component_index}] = {value} is invalid; scale components must be finite and > 0.0"
            ),
        }
    }
}

impl std::error::Error for BodyMutationError {}

/// Applies a non-uniform `scale` to every vertex in `vertices`, about the
/// origin, and returns the result as a new buffer — `vertices` is not
/// mutated.
///
/// - `position' = [position.x * scale[0], position.y * scale[1], position.z * scale[2]]`.
///   See the module-level doc comment for the asset-authoring assumption
///   (shared origin/pivot) this relies on.
/// - `normal' = normalize(normal / scale)` component-wise, which is the
///   correct transform for a diagonal non-uniform scale matrix. If the
///   divided result has near-zero length (possible with extreme scale
///   ratios), the original, untransformed normal is returned instead of
///   producing NaN or a zero vector.
/// - `uv`, `bone_indices`, and `bone_weights` pass through unchanged.
///
/// # Errors
/// Returns [`BodyMutationError::InvalidScale`] if any component of `scale`
/// is `<= 0.0` or non-finite (NaN/infinite), naming the first such
/// component found (by index). No output is produced in that case.
pub fn mutate_skin_vertices(
    vertices: &[SkinnedVertex],
    scale: [f32; 3],
) -> Result<Vec<SkinnedVertex>, BodyMutationError> {
    for (component_index, &value) in scale.iter().enumerate() {
        if !(value.is_finite() && value > 0.0) {
            return Err(BodyMutationError::InvalidScale { component_index, value });
        }
    }

    // Near-zero-length guard threshold for the post-divide normal (squared
    // length, before renormalizing), i.e. the divided vector's length must
    // exceed 1% of a unit vector's length to be trusted. A well-formed
    // unit input normal divided by "ordinary" scale factors comfortably
    // clears this; extreme per-axis scale ratios (e.g. a component scaled
    // by ~1000x) can shrink a divided component enough to fall below it,
    // which is exactly the degenerate case this guard exists to catch.
    const MIN_NORMAL_LENGTH_SQ: f32 = 1e-4;

    let mutated = vertices
        .iter()
        .map(|v| {
            let position = [
                v.position[0] * scale[0],
                v.position[1] * scale[1],
                v.position[2] * scale[2],
            ];

            let divided = [
                v.normal[0] / scale[0],
                v.normal[1] / scale[1],
                v.normal[2] / scale[2],
            ];
            let len_sq = divided[0] * divided[0] + divided[1] * divided[1] + divided[2] * divided[2];
            let normal = if len_sq.is_finite() && len_sq > MIN_NORMAL_LENGTH_SQ {
                let inv_len = 1.0 / len_sq.sqrt();
                [divided[0] * inv_len, divided[1] * inv_len, divided[2] * inv_len]
            } else {
                v.normal
            };

            SkinnedVertex {
                position,
                normal,
                uv: v.uv,
                bone_indices: v.bone_indices,
                bone_weights: v.bone_weights,
            }
        })
        .collect();

    Ok(mutated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(position: [f32; 3], normal: [f32; 3]) -> SkinnedVertex {
        SkinnedVertex {
            position,
            normal,
            uv: [0.25, 0.75],
            bone_indices: [1, 2, 3, 4],
            bone_weights: [0.4, 0.3, 0.2, 0.1],
        }
    }

    fn normal_len(n: [f32; 3]) -> f32 {
        (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt()
    }

    #[test]
    fn identity_scale_leaves_positions_unchanged_and_normals_unit_length() {
        let input = vec![
            v([1.0, 2.0, 3.0], [0.0, 1.0, 0.0]),
            v([-4.0, 0.5, 7.0], [1.0, 0.0, 0.0]),
        ];

        let out = mutate_skin_vertices(&input, [1.0, 1.0, 1.0]).expect("identity scale is valid");

        assert_eq!(out.len(), input.len());
        for (o, i) in out.iter().zip(input.iter()) {
            for k in 0..3 {
                assert!((o.position[k] - i.position[k]).abs() < 1e-6);
            }
            assert!((normal_len(o.normal) - 1.0).abs() < 1e-5);
            assert_eq!(o.uv, i.uv);
            assert_eq!(o.bone_indices, i.bone_indices);
            assert_eq!(o.bone_weights, i.bone_weights);
        }
    }

    #[test]
    fn non_uniform_scale_produces_expected_scaled_positions() {
        let input = vec![
            v([1.0, 1.0, 1.0], [0.0, 1.0, 0.0]),
            v([2.0, -3.0, 4.0], [0.0, 1.0, 0.0]),
            v([0.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ];
        let scale = [2.0, 1.0, 0.5];

        let out = mutate_skin_vertices(&input, scale).expect("valid scale");

        let expected_positions = [[2.0, 1.0, 0.5], [4.0, -3.0, 2.0], [0.0, 0.0, 0.0]];
        for (o, expected) in out.iter().zip(expected_positions.iter()) {
            for k in 0..3 {
                assert!(
                    (o.position[k] - expected[k]).abs() < 1e-6,
                    "position {:?} != expected {:?}",
                    o.position,
                    expected
                );
            }
        }
    }

    #[test]
    fn normals_remain_unit_length_after_non_axis_aligned_non_uniform_scale() {
        // A non-axis-aligned unit normal.
        let raw = [1.0f32, 1.0, 1.0];
        let len = normal_len(raw);
        let unit = [raw[0] / len, raw[1] / len, raw[2] / len];

        let input = vec![v([0.0, 0.0, 0.0], unit)];
        let scale = [2.0, 1.0, 0.5];

        let out = mutate_skin_vertices(&input, scale).expect("valid scale");

        assert!((normal_len(out[0].normal) - 1.0).abs() < 1e-5);

        // Sanity: the transformed normal should equal normalize(unit / scale).
        let divided = [unit[0] / scale[0], unit[1] / scale[1], unit[2] / scale[2]];
        let divided_len = normal_len(divided);
        let expected = [divided[0] / divided_len, divided[1] / divided_len, divided[2] / divided_len];
        for k in 0..3 {
            assert!((out[0].normal[k] - expected[k]).abs() < 1e-5);
        }
    }

    #[test]
    fn each_invalid_scale_component_is_rejected_with_correct_index() {
        let input = vec![v([1.0, 2.0, 3.0], [0.0, 1.0, 0.0])];

        let cases: [(usize, f32); 4] = [
            (0, 0.0),
            (1, -1.0),
            (2, f32::NAN),
            (0, f32::INFINITY),
        ];

        for (bad_index, bad_value) in cases {
            let mut scale = [1.0f32, 1.0, 1.0];
            scale[bad_index] = bad_value;

            let result = mutate_skin_vertices(&input, scale);
            match result {
                Err(BodyMutationError::InvalidScale { component_index, value }) => {
                    assert_eq!(component_index, bad_index);
                    if bad_value.is_nan() {
                        assert!(value.is_nan());
                    } else {
                        assert_eq!(value, bad_value);
                    }
                }
                Ok(_) => panic!("invalid scale must be rejected"),
            }
        }
    }

    #[test]
    fn extreme_scale_ratio_falls_back_to_original_normal_without_nan_or_zero() {
        // A unit normal pointing purely along the axis that gets scaled by
        // 1000x. Dividing by 1000 shrinks its post-divide length to 1/1000
        // of a unit vector's — well under the near-zero-length guard's
        // threshold — which is exactly the degenerate case the fallback
        // exists to catch. Without the fallback this would still normalize
        // "successfully" to a unit vector, but a vanishingly-supported one;
        // the point of this test is confirming the fallback path is what
        // actually runs, not merely that *some* unit vector comes out.
        let extreme_normal = [1.0f32, 0.0, 0.0];
        let input = vec![v([1.0, 2.0, 3.0], extreme_normal)];
        let scale = [1000.0, 0.001, 1.0];

        let out = mutate_skin_vertices(&input, scale).expect("valid scale");

        let n = out[0].normal;
        assert!(n[0].is_finite() && n[1].is_finite() && n[2].is_finite(), "normal must not be NaN/inf: {n:?}");
        let len = normal_len(n);
        assert!(len > 1e-6, "normal must not be zero-length: {n:?}");

        // Confirm the fallback path actually triggered: the post-divide
        // vector's squared length must indeed fall under the guard's
        // threshold for this input.
        let divided = [
            extreme_normal[0] / scale[0],
            extreme_normal[1] / scale[1],
            extreme_normal[2] / scale[2],
        ];
        let divided_len_sq =
            divided[0] * divided[0] + divided[1] * divided[1] + divided[2] * divided[2];
        assert!(divided_len_sq < 1e-4, "test setup should trigger the fallback guard, got len_sq={divided_len_sq}");

        // The fallback returns the original (untransformed) normal
        // exactly, which is already unit-length here.
        assert_eq!(n, extreme_normal, "fallback path should return the original normal untouched");
        assert!((len - 1.0).abs() < 1e-6);
    }

    #[test]
    fn input_slice_is_not_mutated() {
        let input = vec![v([1.0, 2.0, 3.0], [0.0, 1.0, 0.0])];
        let snapshot_pos = input[0].position;
        let snapshot_normal = input[0].normal;

        let _ = mutate_skin_vertices(&input, [2.0, 3.0, 4.0]).expect("valid scale");

        assert_eq!(input[0].position, snapshot_pos);
        assert_eq!(input[0].normal, snapshot_normal);
    }
}
