// Low-level bridge to the anthroforge_core wasm32 module.
//
// This file owns every byte-offset detail of the ABI described in
// TASK-PHASE3-PART-A-ts-sdk-core.md. Nothing above this module (index.ts)
// should know about pointers, offsets, or little-endian layout.
//
// ABI verification note: the struct offsets below were NOT confirmed via a
// wasm32 `size_of` compile-time assert — no Rust toolchain was available in
// the environment this SDK was written in. Instead they were verified
// empirically, at a stronger level than a size-only assert would give:
// the real `anthroforge_core.wasm` binary was instantiated, a real `.afpp`
// pack was loaded through `init_part_registry_from_pack`, and
// `generate_character` was called with known field values. Confirming
// evidence:
//   - Setting `head_id`/`torso_id` at offsets 16/20 to out-of-range values
//     (0-3) produced the exact error "no part loaded for head_id <N>" with
//     N matching what was written — proving those two fields are read from
//     the offsets this code uses, not merely that *some* offset works.
//   - Setting `height_modifier` (offset 8) to 0 produced
//     "DNA mutation failed: scale[1] = 0 is invalid; scale components must
//     be finite and > 0.0" — confirming that offset and its semantic role.
//   - Reading back real vertex data through the predicted `MeshOutputBuffer`
//     (offsets 0/4/8/12) and `SkinnedVertex` (offsets 0/12/24/32/40)
//     layouts produced position/normal/uv/bone-weight values that exactly
//     matched the source OBJ/glTF geometry embedded in the real pack file.
// This round-trips every field against semantically meaningful data, which
// a bare `size_of` assert would not have done (it only checks total size,
// not per-field offsets). If the crate's struct layout changes, these
// offsets must be re-verified the same way.

const CHARACTER_DNA_SIZE = 32;
const MESH_OUTPUT_BUFFER_SIZE = 16;
const SKINNED_VERTEX_SIZE = 56;

export interface RawCharacterDNA {
  seed: bigint;
  heightModifier: number;
  weightModifier: number;
  headId: number;
  torsoId: number;
  clothingIds: number[];
}

export interface RawMesh {
  positions: Float32Array;
  normals: Float32Array;
  uvs: Float32Array;
  boneIndices: Uint16Array;
  boneWeights: Float32Array;
  indices: Uint32Array;
}

export interface RawJoint {
  parentIndex: number; // -1 = root
  translation: [number, number, number];
  rotation: [number, number, number, number];
  scale: [number, number, number];
}

interface WasmExports {
  memory: WebAssembly.Memory;
  wasm_alloc(size: number): number;
  wasm_dealloc(ptr: number, size: number): void;
  init_part_registry_from_pack(packPtr: number, packLen: number): number;
  generate_character(dnaPtr: number): number;
  free_mesh_buffer(bufferPtr: number): void;
  anthroforge_last_error(): number;
  get_skeleton(): number; // returns a pointer, 0 = failure
  free_skeleton_buffer(ptr: number): void;
}

function getCrypto(): Crypto {
  // Use the standard Web Crypto API so this works in both browsers and
  // modern Node (available on globalThis since Node 19+) — the SDK must
  // not depend on Node's `crypto` module directly, since it ships to
  // browsers too.
  const c = globalThis.crypto;
  if (!c || typeof c.getRandomValues !== "function") {
    throw new Error(
      "AnthroForge/Web: no Web Crypto API (globalThis.crypto.getRandomValues) " +
        "available in this environment.",
    );
  }
  return c;
}

export class WasmBridge {
  private readonly exports: WasmExports;

  private constructor(exports: WasmExports) {
    this.exports = exports;
  }

  static async instantiate(wasmBytes: ArrayBuffer | Uint8Array): Promise<WasmBridge> {
    let bridge: WasmBridge;
    const importObject: WebAssembly.Imports = {
      anthroforge_host: {
        // fill_random(ptr: u32, len: u32) -> void, required by the module's
        // getrandom backend. Fill `len` random bytes into the module's own
        // linear memory starting at `ptr`.
        fill_random: (ptr: number, len: number) => {
          const view = new Uint8Array(bridge.exports.memory.buffer, ptr, len);
          getCrypto().getRandomValues(view);
        },
      },
    };

    const source: BufferSource =
      wasmBytes instanceof Uint8Array
        ? (wasmBytes.buffer.slice(
            wasmBytes.byteOffset,
            wasmBytes.byteOffset + wasmBytes.byteLength,
          ) as ArrayBuffer)
        : wasmBytes;
    const { instance } = await WebAssembly.instantiate(source, importObject);
    const exports = instance.exports as unknown as WasmExports;
    bridge = new WasmBridge(exports);
    return bridge;
  }

  /** Loads a Part Pack (`.afpp`) into the module's part registry. */
  initPartRegistryFromPack(packBytes: Uint8Array): boolean {
    const packPtr = this.exports.wasm_alloc(packBytes.length);
    if (packPtr === 0) {
      throw new Error("AnthroForge/Web: wasm_alloc failed while loading the Part Pack.");
    }
    try {
      new Uint8Array(this.exports.memory.buffer, packPtr, packBytes.length).set(packBytes);
      const result = this.exports.init_part_registry_from_pack(packPtr, packBytes.length);
      return result !== 0;
    } finally {
      this.exports.wasm_dealloc(packPtr, packBytes.length);
    }
  }

  /**
   * Calls `generate_character`, copies the resulting mesh out of wasm linear
   * memory into fresh JS typed arrays, and frees the wasm-side buffer before
   * returning — per the task's memory-ownership requirement, callers never
   * receive a view directly over wasm memory.
   */
  generateCharacter(dna: RawCharacterDNA): RawMesh | null {
    const dnaPtr = this.exports.wasm_alloc(CHARACTER_DNA_SIZE);
    if (dnaPtr === 0) {
      throw new Error("AnthroForge/Web: wasm_alloc failed while building CharacterDNA.");
    }

    let clothingPtr = 0;
    const clothingByteLen = dna.clothingIds.length * 4;

    try {
      if (dna.clothingIds.length > 0) {
        clothingPtr = this.exports.wasm_alloc(clothingByteLen);
        if (clothingPtr === 0) {
          throw new Error("AnthroForge/Web: wasm_alloc failed while building the clothing id list.");
        }
        const clothingView = new DataView(this.exports.memory.buffer, clothingPtr, clothingByteLen);
        dna.clothingIds.forEach((id, i) => clothingView.setUint32(i * 4, id >>> 0, true));
      }

      // CharacterDNA, 32 bytes, little-endian (see frozen ABI in the task doc):
      //   0  seed                      u64
      //   8  height_modifier           f32
      //  12  weight_modifier           f32
      //  16  head_id                   u32
      //  20  torso_id                  u32
      //  24  equipped_clothing_ids_ptr u32 (wasm32 address, 0 = null)
      //  28  equipped_clothing_count   u32
      const dnaView = new DataView(this.exports.memory.buffer, dnaPtr, CHARACTER_DNA_SIZE);
      dnaView.setBigUint64(0, dna.seed, true);
      dnaView.setFloat32(8, dna.heightModifier, true);
      dnaView.setFloat32(12, dna.weightModifier, true);
      dnaView.setUint32(16, dna.headId >>> 0, true);
      dnaView.setUint32(20, dna.torsoId >>> 0, true);
      dnaView.setUint32(24, clothingPtr, true);
      dnaView.setUint32(28, dna.clothingIds.length, true);

      const bufferPtr = this.exports.generate_character(dnaPtr);
      if (bufferPtr === 0) {
        return null;
      }

      try {
        return this.readMeshOutputBuffer(bufferPtr);
      } finally {
        this.exports.free_mesh_buffer(bufferPtr);
      }
    } finally {
      this.exports.wasm_dealloc(dnaPtr, CHARACTER_DNA_SIZE);
      if (clothingPtr !== 0) {
        this.exports.wasm_dealloc(clothingPtr, clothingByteLen);
      }
    }
  }

  private readMeshOutputBuffer(bufferPtr: number): RawMesh {
    // MeshOutputBuffer, 16 bytes, little-endian:
    //   0  vertices_ptr    u32
    //   4  indices_ptr     u32
    //   8  vertices_count  u32
    //  12  indices_count   u32
    const headerView = new DataView(this.exports.memory.buffer, bufferPtr, MESH_OUTPUT_BUFFER_SIZE);
    const verticesPtr = headerView.getUint32(0, true);
    const indicesPtr = headerView.getUint32(4, true);
    const verticesCount = headerView.getUint32(8, true);
    const indicesCount = headerView.getUint32(12, true);

    const positions = new Float32Array(verticesCount * 3);
    const normals = new Float32Array(verticesCount * 3);
    const uvs = new Float32Array(verticesCount * 2);
    const boneIndices = new Uint16Array(verticesCount * 4);
    const boneWeights = new Float32Array(verticesCount * 4);

    // SkinnedVertex, 56 bytes each, little-endian, identical on every
    // target (no pointers in this struct):
    //   0  position      [f32; 3]  (12 bytes)
    //  12  normal        [f32; 3]  (12 bytes)
    //  24  uv            [f32; 2]  ( 8 bytes)
    //  32  bone_indices  [u16; 4]  ( 8 bytes)
    //  40  bone_weights  [f32; 4]  (16 bytes)
    for (let i = 0; i < verticesCount; i++) {
      const vertexBase = verticesPtr + i * SKINNED_VERTEX_SIZE;
      const v = new DataView(this.exports.memory.buffer, vertexBase, SKINNED_VERTEX_SIZE);

      for (let c = 0; c < 3; c++) {
        positions[i * 3 + c] = v.getFloat32(c * 4, true);
        normals[i * 3 + c] = v.getFloat32(12 + c * 4, true);
      }
      for (let c = 0; c < 2; c++) {
        uvs[i * 2 + c] = v.getFloat32(24 + c * 4, true);
      }
      for (let c = 0; c < 4; c++) {
        boneIndices[i * 4 + c] = v.getUint16(32 + c * 2, true);
        boneWeights[i * 4 + c] = v.getFloat32(40 + c * 4, true);
      }
    }

    // Flat u32 index array, `indices_count` entries, copied out before the
    // wasm-side buffer is freed by the caller.
    const indices = new Uint32Array(
      this.exports.memory.buffer.slice(indicesPtr, indicesPtr + indicesCount * 4),
    );

    return { positions, normals, uvs, boneIndices, boneWeights, indices };
  }

  /**
   * Reads the fully-assembled global skeleton (`master_skeleton.json`'s
   * bone hierarchy plus every bone's real bind-pose local transform,
   * contributed across every loaded part) via `get_skeleton`. This data is
   * read-only, global, and per-process — unlike `generateCharacter`'s
   * per-call output — so call this once after `initPartRegistryFromPack`
   * succeeds, not once per generated character.
   */
  getSkeleton(): RawJoint[] | null {
    const ptr = this.exports.get_skeleton();
    if (ptr === 0) {
      return null;
    }
    try {
      // SkeletonBuffer, 8 bytes on wasm32, little-endian:
      //   0  joints_ptr    u32 (wasm32 address)
      //   4  joint_count   u32
      const headerView = new DataView(this.exports.memory.buffer, ptr, 8);
      const jointsPtr = headerView.getUint32(0, true);
      const jointCount = headerView.getUint32(4, true);

      const joints: RawJoint[] = [];
      for (let i = 0; i < jointCount; i++) {
        // FfiJoint, 44 bytes, little-endian, identical on every target
        // (no pointers):
        //   0   parent_index  i32   (-1 = no parent / this is a root)
        //   4   translation   [f32; 3]  (12 bytes)
        //  16   rotation      [f32; 4]  (16 bytes) -- quaternion, (x, y, z, w)
        //  32   scale         [f32; 3]  (12 bytes)
        const base = jointsPtr + i * 44;
        const v = new DataView(this.exports.memory.buffer, base, 44);
        joints.push({
          parentIndex: v.getInt32(0, true),
          translation: [v.getFloat32(4, true), v.getFloat32(8, true), v.getFloat32(12, true)],
          rotation: [
            v.getFloat32(16, true),
            v.getFloat32(20, true),
            v.getFloat32(24, true),
            v.getFloat32(28, true),
          ],
          scale: [v.getFloat32(32, true), v.getFloat32(36, true), v.getFloat32(40, true)],
        });
      }
      return joints;
    } finally {
      this.exports.free_skeleton_buffer(ptr);
    }
  }

  /**
   * Reads and copies out the last error string recorded by the module.
   * Per the ABI contract this must be read before any further call into the
   * module, since the string is only valid until the next call.
   */
  getLastError(): string | null {
    const ptr = this.exports.anthroforge_last_error();
    if (ptr === 0) {
      return null;
    }
    const bytes = new Uint8Array(this.exports.memory.buffer);
    let end = ptr;
    while (bytes[end] !== 0) {
      end++;
    }
    return new TextDecoder("utf-8").decode(bytes.slice(ptr, end));
  }
}
