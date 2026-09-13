import * as THREE from "three";
import type { GeneratedCharacter } from "@anthroforge/web";
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
export declare function toBufferGeometry(character: GeneratedCharacter): THREE.BufferGeometry;
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
export declare function hasAtlas(character: GeneratedCharacter): boolean;
