# Running the AnthroForge web demo locally

This zip ships a **pre-built** `@anthroforge/web` package (wasm + JS in
`packages/web/dist/`), so you do not need Rust/cargo or a JS build step
to see it run — you just need a static file server (the browser blocks
`fetch()` of the `.wasm`/`.afpp` files from a bare `file://` page).

One thing was missing from the zip and has been added: `real_test.afpp`
at the project root. The demo page (`packages/web/demo/index.html`)
expects this file three directories up from itself. It's a small
"part pack" binary built from the sample assets in
`rust-core/tests/fixtures/real_pack_e2e/` (a head, legs, arms, torso,
and a skeleton) — it did not exist anywhere in the original zip, so I
built it myself in Node (there's no Rust toolchain available in the
environment I unpacked this in) by re-implementing the exact byte
layout documented in `rust-core/src/bin/pack_builder.rs`. I then
verified it for real: I ran the package's own test suite
(`packages/web/src/index.test.mjs`, `node --test`) against it and the
real built `anthroforge_core.wasm`, and all 5 tests passed, including
`generate()` returning a real non-null mesh from real part ids.

## Steps

1. Unzip this project.
2. From the project root, start any static server, e.g.:
   ```sh
   npx serve .
   # or
   python3 -m http.server 8000
   ```
3. Open the demo in your browser:
   `http://localhost:<port>/packages/web/demo/index.html`
4. Move the Height/Weight sliders and click **Generate**. You should
   see a wireframe render on the canvas plus real generation-time,
   vertex, and index counts.

## Update: a real 3D showcase (`showcase-3d/`)

The 2D wireframe demo above only ever renders `real_test.afpp`'s tiny
placeholder fixture (a 3-vertex "head" triangle). To show what this
pipeline looks like with an actual character, I pulled KayKit's CC0
"Adventurers" Knight from
[github.com/KayKit-Game-Assets/KayKit-Character-Pack-Adventures-1.0](https://github.com/KayKit-Game-Assets/KayKit-Character-Pack-Adventures-1.0)
and built two real AnthroForge parts from its real glTF data:

- `1001_head.glb` — the Knight's head mesh
- `2001_body.glb` — its arms, torso, and legs merged into one mesh,
  sharing the Knight's real 41-bone skin
- `master_skeleton.json` — a name→index map of all 41 real bone names
- packed into `kaykit_character.afpp`

This was **not** just dropped in — I verified it against the real,
unmodified `anthroforge_core.wasm` in a Node script before writing any
demo page: `init()` + `generate({headId:1001, torsoId:2001, ...})`
returned a real non-null mesh (3716 vertices, 12444 indices).

Open `showcase-3d/index.html` (via the same static server, from the
project root) for a textured, lit, orbit-controllable three.js render
of that real mesh, with the same height/weight sliders.

**One real limitation I found while building this, worth knowing:**
`packages/web/src/index.ts` declares `getSkeleton()` for skinned/posed
rendering, and `PROJECT_STATE.md` describes it as "done, verified for
real." But the actual `anthroforge_core.wasm` shipped in this zip
predates that work — I checked its exports directly
(`WebAssembly.Module.exports`) and it has 14 exports, not the 17 the
notes describe, with no `get_skeleton`/`free_skeleton_buffer`. So
`showcase-3d` renders the character in its authored bind pose as a
static mesh, not a posed/animated `THREE.SkinnedMesh` — that part of
the pipeline needs `anthroforge_core.wasm` rebuilt from `rust-core/`
with a real Rust toolchain, which wasn't available in the sandbox I
used to assemble this.

The mesh orientation and camera framing in `showcase-3d/index.html`
are my best real-data-checked estimate (I confirmed the glTF's own
joint translations stack upward along +Y, so no axis correction is
applied) but I haven't been able to render this in an actual browser
from this environment — if the character looks off on your screen,
that's the part to sanity-check first.

## What this does and doesn't cover

- This demo only exercises `@anthroforge/web` (the core wasm SDK) with
  a flat 2D canvas wireframe — it's the only demo page shipped in the
  zip.
- `packages/web-three/` (the three.js adapter — skinned meshes, real
  `THREE.BufferGeometry`/`THREE.SkinnedMesh` output) has a real,
  passing test suite (`node --test src/index.test.mjs` from that
  folder) but **no demo HTML page of its own**. To showcase that part
  in a browser you'd need to write a small three.js scene that calls
  `toBufferGeometry()`/`toSkinnedMesh()` — happy to build that next if
  you want a fuller 3D showcase.
