// Hand-built, non-trivial test fixture for skeleton extraction / skinning.
//
// The checked-in `rust-core/tests/fixtures/real_pack_e2e/` pack was
// trimmed to a single identity-transform, parentless "root" joint, which
// would pass even a completely broken hierarchy-builder or
// updateMatrixWorld-less THREE.Skeleton construction silently (identity
// bones deform identically whether the code is right or wrong).
//
// This module instead reproduces, byte-for-byte, the *values* of
// `rust-core/src/gltf_loader.rs`'s `build_hierarchy_test_glb()` Rust unit
// test fixture (a real root -> spine -> head chain, one decomposed-TRS
// translation, one raw-Matrix rotation+non-uniform-scale) as a real,
// loadable `.glb` built here in JS, then packs it into a real `.afpp`
// Part Pack per the frozen format documented in `rust-core/src/bin/
// pack_builder.rs`'s header comment:
//
//   offset 0   : magic       b"AFPP"           (4 bytes, ASCII, no NUL)
//   offset 4   : version     u32 LE = 1
//   offset 8   : part_count  u32 LE (N, N >= 1)
//   offset 12  : skel_len    u32 LE
//   offset 16  : skeleton    skel_len raw bytes (master_skeleton.json,
//                            verbatim, UTF-8)
//   then N fixed-size 13-byte index entries, back-to-back, no padding:
//                part_id     u32 LE
//                src_type    u8   (0 = OBJ, 1 = glTF/GLB)
//                data_offset u32 LE (absolute offset from start of buffer)
//                data_len    u32 LE
//   then the N parts' raw source bytes.

/** Little-endian f32 bytes. */
function f32le(n) {
  const buf = new ArrayBuffer(4);
  new DataView(buf).setFloat32(0, n, true);
  return new Uint8Array(buf);
}

/** Little-endian u32 bytes. */
function u32le(n) {
  const buf = new ArrayBuffer(4);
  new DataView(buf).setUint32(0, n >>> 0, true);
  return new Uint8Array(buf);
}

/** Little-endian u16 bytes. */
function u16le(n) {
  const buf = new ArrayBuffer(2);
  new DataView(buf).setUint16(0, n, true);
  return new Uint8Array(buf);
}

function concatBytes(chunks) {
  const total = chunks.reduce((sum, c) => sum + c.length, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const c of chunks) {
    out.set(c, offset);
    offset += c.length;
  }
  return out;
}

/**
 * Builds a real, minimal, valid `.glb` with a real 3-joint
 * root -> spine -> head chain and non-identity bind-pose transforms on
 * two of the three joints -- the exact same values as
 * `build_hierarchy_test_glb()` in `rust-core/src/gltf_loader.rs`, so this
 * fixture's expected joint values below are cross-checked against that
 * Rust unit test's own assertions, not invented independently.
 *
 * - root:  identity (no node transform at all)
 * - spine: translation (0, 1.5, 0), authored via decomposed TRS
 * - head:  translation (0, 0, 2.5), rotation = 90 degrees about +Z,
 *          scale (2, 3, 1), authored as a single raw column-major Matrix
 *          (must come back correctly decomposed)
 */
export function buildHierarchyTestGlb() {
  const positions = [
    [0.0, 0.0, 0.0],
    [1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
  ];
  const normals = [
    [0.0, 0.0, 1.0],
    [0.0, 0.0, 1.0],
    [0.0, 0.0, 1.0],
  ];
  const texcoords = [
    [0.0, 0.0],
    [1.0, 0.0],
    [0.0, 1.0],
  ];
  // All fully weighted onto local joint 0 ("root") -- vertex skinning
  // weights are irrelevant to this fixture, only the joint
  // hierarchy/bind-pose extraction is under test.
  const joints = [
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
  ];
  const weights = [
    [1.0, 0.0, 0.0, 0.0],
    [1.0, 0.0, 0.0, 0.0],
    [1.0, 0.0, 0.0, 0.0],
  ];
  const indices = [0, 1, 2];

  const binChunks = [];
  for (const p of positions) for (const c of p) binChunks.push(f32le(c));
  const normalsOffset = binChunks.length * 4;
  for (const n of normals) for (const c of n) binChunks.push(f32le(c));
  const texcoordsOffset = normalsOffset + normals.length * 3 * 4;
  for (const t of texcoords) for (const c of t) binChunks.push(f32le(c));
  const jointsOffset = texcoordsOffset + texcoords.length * 2 * 4;
  for (const j of joints) for (const c of j) binChunks.push(new Uint8Array([c]));
  const weightsOffset = jointsOffset + joints.length * 4;
  for (const w of weights) for (const c of w) binChunks.push(f32le(c));
  const indicesOffset = weightsOffset + weights.length * 4 * 4;
  for (const i of indices) binChunks.push(u16le(i));
  const indicesByteLen = indices.length * 2;

  let bin = concatBytes(binChunks);
  const bufferByteLen = bin.length;
  while (bin.length % 4 !== 0) {
    bin = concatBytes([bin, new Uint8Array([0])]);
  }

  // head's bind pose, authored as a raw Matrix: 90-degree rotation about
  // +Z, non-uniform scale (2, 3, 1), translation (0, 0, 2.5).
  // Column-major, flattened -- identical to build_hierarchy_test_glb's.
  const headMatrix = [
    0.0, 2.0, 0.0, 0.0, // column 0
    -3.0, 0.0, 0.0, 0.0, // column 1
    0.0, 0.0, 1.0, 0.0, // column 2
    0.0, 0.0, 2.5, 1.0, // column 3 (translation)
  ];

  const json = {
    asset: { version: "2.0" },
    scene: 0,
    scenes: [{ nodes: [0] }],
    nodes: [
      { mesh: 0, skin: 0 },
      { name: "root", children: [2] },
      { name: "spine", translation: [0.0, 1.5, 0.0], children: [3] },
      { name: "head", matrix: headMatrix },
    ],
    meshes: [
      {
        primitives: [
          {
            attributes: {
              POSITION: 0,
              NORMAL: 1,
              TEXCOORD_0: 2,
              JOINTS_0: 3,
              WEIGHTS_0: 4,
            },
            indices: 5,
            mode: 4,
          },
        ],
      },
    ],
    skins: [{ joints: [1, 2, 3] }],
    accessors: [
      {
        bufferView: 0,
        componentType: 5126,
        count: 3,
        type: "VEC3",
        min: [0.0, 0.0, 0.0],
        max: [1.0, 1.0, 0.0],
      },
      { bufferView: 1, componentType: 5126, count: 3, type: "VEC3" },
      { bufferView: 2, componentType: 5126, count: 3, type: "VEC2" },
      { bufferView: 3, componentType: 5121, count: 3, type: "VEC4" },
      { bufferView: 4, componentType: 5126, count: 3, type: "VEC4" },
      { bufferView: 5, componentType: 5123, count: indices.length, type: "SCALAR" },
    ],
    bufferViews: [
      { buffer: 0, byteOffset: 0, byteLength: normalsOffset },
      { buffer: 0, byteOffset: normalsOffset, byteLength: texcoordsOffset - normalsOffset },
      { buffer: 0, byteOffset: texcoordsOffset, byteLength: jointsOffset - texcoordsOffset },
      { buffer: 0, byteOffset: jointsOffset, byteLength: weightsOffset - jointsOffset },
      { buffer: 0, byteOffset: weightsOffset, byteLength: indicesOffset - weightsOffset },
      { buffer: 0, byteOffset: indicesOffset, byteLength: indicesByteLen },
    ],
    buffers: [{ byteLength: bufferByteLen }],
  };

  let jsonBytes = new TextEncoder().encode(JSON.stringify(json));
  if (jsonBytes.length % 4 !== 0) {
    const pad = 4 - (jsonBytes.length % 4);
    jsonBytes = concatBytes([jsonBytes, new Uint8Array(pad).fill(0x20)]); // space-pad, per glTF spec
  }

  const totalLen = 12 + 8 + jsonBytes.length + 8 + bin.length;
  const glb = concatBytes([
    new TextEncoder().encode("glTF"),
    u32le(2),
    u32le(totalLen),
    u32le(jsonBytes.length),
    new TextEncoder().encode("JSON"),
    jsonBytes,
    u32le(bin.length),
    new TextEncoder().encode("BIN\0"),
    bin,
  ]);
  return glb;
}

/**
 * Builds a real `.afpp` Part Pack containing:
 *  - `headPartId` (a real `.glb`, `buildHierarchyTestGlb()`'s real
 *    root -> spine -> head chain, contributing all 3 global joints), and
 *  - `torsoPartId` (a trivial real `.obj`, rigidly bound to the OBJ
 *    loader's fixed "root" bone name, matching the GLB part's own
 *    identity-transform, parentless "root" joint so the two parts agree).
 *
 * `master_skeleton.json` names exactly those 3 joints (`root`: 0,
 * `spine`: 1, `head`: 2), so this pack is loadable end-to-end and
 * `generate()` can be called with `headId: headPartId, torsoId:
 * torsoPartId` to get a real merged, skinned character.
 */
export function buildHierarchyFixturePack() {
  const headPartId = 1001;
  const torsoPartId = 2001;

  const skeletonJson = new TextEncoder().encode(
    JSON.stringify({ root: 0, spine: 1, head: 2 }),
  );
  const glbBytes = buildHierarchyTestGlb();
  const objBytes = new TextEncoder().encode(
    "v 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 0.0 1.0 0.0\nf 1 2 3\n",
  );

  const HEADER_LEN = 16;
  const INDEX_ENTRY_LEN = 13;
  const partCount = 2;
  const indexLen = partCount * INDEX_ENTRY_LEN;

  const dataRegionStart = HEADER_LEN + skeletonJson.length + indexLen;
  const glbOffset = dataRegionStart;
  const objOffset = glbOffset + glbBytes.length;

  const chunks = [];
  chunks.push(new TextEncoder().encode("AFPP"));
  chunks.push(u32le(1)); // version
  chunks.push(u32le(partCount));
  chunks.push(u32le(skeletonJson.length));
  chunks.push(skeletonJson);

  // Index entry 0: the glTF/GLB part (head).
  chunks.push(u32le(headPartId));
  chunks.push(new Uint8Array([1])); // src_type = 1 (glTF/GLB)
  chunks.push(u32le(glbOffset));
  chunks.push(u32le(glbBytes.length));

  // Index entry 1: the OBJ part (torso).
  chunks.push(u32le(torsoPartId));
  chunks.push(new Uint8Array([0])); // src_type = 0 (OBJ)
  chunks.push(u32le(objOffset));
  chunks.push(u32le(objBytes.length));

  // Data region, in the same order as the index table.
  chunks.push(glbBytes);
  chunks.push(objBytes);

  const packBytes = concatBytes(chunks);

  // Expected joints, cross-checked directly against
  // `hierarchy_glb_extracts_real_parents_and_bind_poses`'s own assertions
  // in rust-core/src/gltf_loader.rs -- not re-derived independently.
  const expectedJoints = [
    {
      parentIndex: -1,
      translation: [0.0, 0.0, 0.0],
      rotation: [0.0, 0.0, 0.0, 1.0],
      scale: [1.0, 1.0, 1.0],
    },
    {
      parentIndex: 0,
      translation: [0.0, 1.5, 0.0],
      rotation: [0.0, 0.0, 0.0, 1.0],
      scale: [1.0, 1.0, 1.0],
    },
    {
      parentIndex: 1,
      translation: [0.0, 0.0, 2.5],
      rotation: [0.0, 0.0, 0.70710678, 0.70710678],
      scale: [2.0, 3.0, 1.0],
    },
  ];

  return { packBytes, headPartId, torsoPartId, expectedJoints };
}
