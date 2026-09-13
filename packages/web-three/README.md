# @anthroforge/web-three

A `three.js` render adapter for [`@anthroforge/web`](../web). Converts a
`GeneratedCharacter` (the output of `@anthroforge/web`'s `generate()`) into
a `THREE.BufferGeometry`.

## Usage

```ts
import { init, generate } from "@anthroforge/web";
import { toBufferGeometry, hasAtlas } from "@anthroforge/web-three";
import * as THREE from "three";

await init({ partPackUrl: "/my-pack.afpp", licenseKey: "..." });

const character = generate({ /* ... */ });
if (character) {
  const geometry = toBufferGeometry(character);
  const material = new THREE.MeshStandardMaterial({ color: 0xcccccc });
  const mesh = new THREE.Mesh(geometry, material);
  scene.add(mesh);
}
```

`toBufferGeometry` only builds the `BufferGeometry` — it does not create a
`THREE.Mesh` or pick a material, so you're free to use whatever material or
mesh type (including a future `THREE.SkinnedMesh`) fits your app.

## Known limitations

### Skinning is not wired up yet

`character.boneIndices` / `character.boneWeights` are exposed on the
returned geometry as ordinary `"skinIndex"` / `"skinWeight"`
`BufferAttribute`s (the standard three.js attribute names), but this
package does **not** build a `THREE.Bone` hierarchy, a `THREE.Skeleton`,
or a working `THREE.SkinnedMesh`. The bone indices are indices into this
crate's own bone-index space, defined by `master_skeleton.json`, and
nothing here maps that space onto real `THREE.Bone` objects yet.
Building that mapping is separate, not-yet-done work. Until then, treat
this geometry as unskinned (static) geometry.

### Texture atlas / texturing is not available yet

`generate()` in `@anthroforge/web` does not currently produce a texture
atlas — `generate_character` on the wasm side only returns mesh geometry
(`MeshOutputBuffer`); atlas generation (`generate_runtime_atlas`) is a
separate wasm export that isn't invoked by this call path. Because of
that, `character.atlasBytes` is always empty and `atlasWidth` /
`atlasHeight` are always `0` today, and this adapter does not fabricate a
placeholder or checkerboard texture to paper over that. Use the exported
`hasAtlas(character)` helper to check before attempting to build a
`THREE.Texture`:

```ts
if (hasAtlas(character)) {
  // build a THREE.Texture from character.atlasBytes / atlasWidth / atlasHeight
} else {
  // no atlas available yet — fall back to a plain material
}
```

`hasAtlas()` will start returning `true` once atlas generation is wired
into `generate()` upstream; no change to this package should be needed at
that point beyond actually consuming the bytes.

## `three` version

`three` is declared as a `peerDependency` (`>=0.160.0`), not a regular
dependency — this package must not bundle its own copy of three.js, and
should use whatever version the consuming app already has installed. The
`BufferGeometry` / `BufferAttribute` APIs this package relies on
(`setAttribute`, `setIndex`, `computeBoundingSphere`) have been stable for
a long time, so `>=0.160.0` was chosen as a reasonably recent floor
(current published `three` at the time of writing is `0.186.0`) rather
than a narrow caret range, since three.js's own versioning (0.x) means a
caret range like `^0.160.0` would only allow `0.160.x` and exclude every
newer release.
