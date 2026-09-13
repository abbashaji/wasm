import * as THREE from "three";
import type { GeneratedCharacter, SkeletonJoint } from "@anthroforge/web";

/**
 * Converts a `GeneratedCharacter` produced by `@anthroforge/web`'s
 * `generate()` into a `THREE.BufferGeometry`.
 *
 * - `positions` / `normals` / `uvs` are mapped directly onto `"position"`
 *   (itemSize 3), `"normal"` (itemSize 3), and `"uv"` (itemSize 2)
 *   `BufferAttribute`s — they are already in the flat, interleaved-free
 *   layout `BufferGeometry.setAttribute` expects, so no reshaping happens
 *   here.
 * - `indices` is set via `geometry.setIndex(...)`.
 * - `geometry.computeBoundingSphere()` is called before returning, since
 *   three.js does not do this automatically and needs it for frustum
 *   culling.
 *
 * Skinning limitation (read before assuming skinned rendering works):
 * `character.boneIndices` / `character.boneWeights` are copied onto this
 * geometry as ordinary `"skinIndex"` / `"skinWeight"` `BufferAttribute`s
 * (the standard three.js attribute names for this data), but this
 * function does **not** build a `THREE.Skeleton`, a `THREE.Bone`
 * hierarchy, or a `THREE.SkinnedMesh`. The bone indices in
 * `character.boneIndices` are indices into this crate's own bone-index
 * space (defined by `master_skeleton.json`), and nothing in this file
 * maps that space onto real `THREE.Bone` objects. Attaching this
 * geometry to a `THREE.SkinnedMesh` as-is will not deform correctly —
 * that requires a separate, not-yet-built task that constructs a
 * `THREE.Bone` hierarchy from `master_skeleton.json` and binds it to a
 * `THREE.Skeleton`. The `"skinIndex"` / `"skinWeight"` attributes are
 * exposed now purely so that future task can consume this geometry
 * without having to modify this function.
 *
 * This function deliberately does not construct a `THREE.Mesh` or apply
 * any material — it returns only the `BufferGeometry` so the caller
 * remains free to choose a material, wrap it in a `THREE.SkinnedMesh`
 * once skinning is implemented, etc.
 */
export function toBufferGeometry(character: GeneratedCharacter): THREE.BufferGeometry {
  const geometry = new THREE.BufferGeometry();

  geometry.setAttribute("position", new THREE.BufferAttribute(character.positions, 3));
  geometry.setAttribute("normal", new THREE.BufferAttribute(character.normals, 3));
  geometry.setAttribute("uv", new THREE.BufferAttribute(character.uvs, 2));

  // Not wired up to a THREE.Skeleton / THREE.Bone hierarchy yet — see the
  // doc comment above. Exposed under the standard three.js skinning
  // attribute names so a future skinning task can pick them up directly.
  geometry.setAttribute("skinIndex", new THREE.BufferAttribute(character.boneIndices, 4));
  geometry.setAttribute("skinWeight", new THREE.BufferAttribute(character.boneWeights, 4));

  geometry.setIndex(new THREE.BufferAttribute(character.indices, 1));

  geometry.computeBoundingSphere();

  return geometry;
}

/**
 * Reports whether `character` carries a real texture atlas.
 *
 * `generate()` in `@anthroforge/web` does not currently wire up atlas
 * generation (`generate_runtime_atlas` is a separate, independently
 * callable wasm export that the `generate()` call path does not invoke),
 * so `character.atlasBytes` is always empty and `atlasWidth` /
 * `atlasHeight` are always `0` today. This function does not fabricate a
 * placeholder texture — it simply reports the real state of the data so
 * a caller can check before attempting to build a `THREE.Texture`, and
 * should be expected to start returning `true` once atlas generation is
 * wired into `generate()` upstream.
 */
export function hasAtlas(character: GeneratedCharacter): boolean {
  return character.atlasWidth > 0 && character.atlasHeight > 0;
}

/**
 * Converts a `GeneratedCharacter` plus the real `SkeletonJoint[]` returned
 * by `@anthroforge/web`'s `getSkeleton()` into a real, bound
 * `THREE.SkinnedMesh` — the follow-up to `toBufferGeometry`'s skinning
 * limitation (see its doc comment above).
 *
 * `skeleton[i]` becomes bone `i`: this ordering is the same global
 * bone-index space `character.boneIndices` already indexes into (per the
 * Rust-side `resolve_bone_indices` remap), so the array must not be
 * reordered, sorted, or filtered.
 */
export function toSkinnedMesh(
  character: GeneratedCharacter,
  skeleton: SkeletonJoint[],
  material?: THREE.Material,
): THREE.SkinnedMesh {
  const geometry = toBufferGeometry(character);

  const bones: THREE.Bone[] = skeleton.map((joint) => {
    const bone = new THREE.Bone();
    bone.position.fromArray(joint.translation);
    // three.js's Quaternion expects (x, y, z, w), exactly the order
    // `rotation` is already in.
    bone.quaternion.fromArray(joint.rotation);
    bone.scale.fromArray(joint.scale);
    return bone;
  });

  const roots: THREE.Bone[] = [];
  skeleton.forEach((joint, i) => {
    if (joint.parentIndex !== -1) {
      bones[joint.parentIndex].add(bones[i]);
    } else {
      roots.push(bones[i]);
    }
  });

  // A well-formed skeleton should have exactly one root. THREE.Skeleton
  // does not actually require a single connected hierarchy, so this is
  // not fatal, but it's a real anomaly in the upstream data worth
  // surfacing rather than silently accepting as normal.
  if (roots.length !== 1) {
    console.warn(
      `toSkinnedMesh: expected exactly 1 root bone (parentIndex === -1), found ${roots.length}`,
    );
  }

  // THREE.Skeleton's constructor (called with only one argument, below)
  // calls calculateInverses() immediately, which reads each bone's
  // matrixWorld. Object3D's matrixWorld is not automatically up to date
  // until something triggers a matrix-world update, so this must run
  // first — skipping it silently produces identity inverse bind matrices
  // and a mesh that never deforms.
  for (const root of roots) {
    root.updateMatrixWorld(true);
  }

  const skeleton3 = new THREE.Skeleton(bones);

  const mesh = new THREE.SkinnedMesh(geometry, material);

  // Adds the bones to the same scene graph as the mesh -- required for
  // skinning to work, and distinct from mesh.bind() below.
  for (const root of roots) {
    mesh.add(root);
  }

  mesh.bind(skeleton3);

  return mesh;
}
