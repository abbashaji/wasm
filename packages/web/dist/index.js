// src/wasm-bridge.ts
var CHARACTER_DNA_SIZE = 32;
var MESH_OUTPUT_BUFFER_SIZE = 16;
var SKINNED_VERTEX_SIZE = 56;
function getCrypto() {
  const c = globalThis.crypto;
  if (!c || typeof c.getRandomValues !== "function") {
    throw new Error(
      "AnthroForge/Web: no Web Crypto API (globalThis.crypto.getRandomValues) available in this environment."
    );
  }
  return c;
}
var WasmBridge = class _WasmBridge {
  exports;
  constructor(exports) {
    this.exports = exports;
  }
  static async instantiate(wasmBytes) {
    let bridge2;
    const importObject = {
      anthroforge_host: {
        // fill_random(ptr: u32, len: u32) -> void, required by the module's
        // getrandom backend. Fill `len` random bytes into the module's own
        // linear memory starting at `ptr`.
        fill_random: (ptr, len) => {
          const view = new Uint8Array(bridge2.exports.memory.buffer, ptr, len);
          getCrypto().getRandomValues(view);
        }
      }
    };
    const source = wasmBytes instanceof Uint8Array ? wasmBytes.buffer.slice(
      wasmBytes.byteOffset,
      wasmBytes.byteOffset + wasmBytes.byteLength
    ) : wasmBytes;
    const { instance } = await WebAssembly.instantiate(source, importObject);
    const exports = instance.exports;
    bridge2 = new _WasmBridge(exports);
    return bridge2;
  }
  /** Loads a Part Pack (`.afpp`) into the module's part registry. */
  initPartRegistryFromPack(packBytes) {
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
  generateCharacter(dna) {
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
  readMeshOutputBuffer(bufferPtr) {
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
    const indices = new Uint32Array(
      this.exports.memory.buffer.slice(indicesPtr, indicesPtr + indicesCount * 4)
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
  getSkeleton() {
    const ptr = this.exports.get_skeleton();
    if (ptr === 0) {
      return null;
    }
    try {
      const headerView = new DataView(this.exports.memory.buffer, ptr, 8);
      const jointsPtr = headerView.getUint32(0, true);
      const jointCount = headerView.getUint32(4, true);
      const joints = [];
      for (let i = 0; i < jointCount; i++) {
        const base = jointsPtr + i * 44;
        const v = new DataView(this.exports.memory.buffer, base, 44);
        joints.push({
          parentIndex: v.getInt32(0, true),
          translation: [v.getFloat32(4, true), v.getFloat32(8, true), v.getFloat32(12, true)],
          rotation: [
            v.getFloat32(16, true),
            v.getFloat32(20, true),
            v.getFloat32(24, true),
            v.getFloat32(28, true)
          ],
          scale: [v.getFloat32(32, true), v.getFloat32(36, true), v.getFloat32(40, true)]
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
  getLastError() {
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
};

// src/index.ts
var bridge = null;
async function init(options) {
  const wasmUrl = new URL("./anthroforge_core.wasm", import.meta.url);
  const [wasmResponse, packResponse] = await Promise.all([
    fetch(wasmUrl),
    fetch(options.partPackUrl)
  ]);
  if (!wasmResponse.ok) {
    throw new Error(
      `AnthroForge/Web: failed to fetch bundled wasm module (${wasmUrl}): ${wasmResponse.status} ${wasmResponse.statusText}`
    );
  }
  if (!packResponse.ok) {
    throw new Error(
      `AnthroForge/Web: failed to fetch Part Pack (${options.partPackUrl}): ${packResponse.status} ${packResponse.statusText}`
    );
  }
  void options.licenseKey;
  const wasmBytes = await wasmResponse.arrayBuffer();
  const packBytes = new Uint8Array(await packResponse.arrayBuffer());
  const newBridge = await WasmBridge.instantiate(wasmBytes);
  const ok = newBridge.initPartRegistryFromPack(packBytes);
  if (!ok) {
    const err = newBridge.getLastError();
    throw new Error(
      `AnthroForge/Web: init_part_registry_from_pack failed${err ? `: ${err}` : ""}`
    );
  }
  bridge = newBridge;
}
function requireBridge() {
  if (!bridge) {
    throw new Error("AnthroForge/Web: init() must be called and awaited before generate().");
  }
  return bridge;
}
function generate(dna) {
  const b = requireBridge();
  const rawDna = {
    seed: dna.seed,
    heightModifier: dna.heightModifier,
    weightModifier: dna.weightModifier,
    headId: dna.headId,
    torsoId: dna.torsoId,
    clothingIds: dna.clothingIds
  };
  const mesh = b.generateCharacter(rawDna);
  if (mesh === null) {
    return null;
  }
  return {
    positions: mesh.positions,
    normals: mesh.normals,
    uvs: mesh.uvs,
    boneIndices: mesh.boneIndices,
    boneWeights: mesh.boneWeights,
    indices: mesh.indices,
    // Not wired into generate_character's path yet — see the doc comment
    // on GeneratedCharacter.atlasBytes above. Deliberately empty, not
    // fabricated placeholder pixel data.
    atlasBytes: new Uint8Array(0),
    atlasWidth: 0,
    atlasHeight: 0
  };
}
function getLastError() {
  return requireBridge().getLastError();
}
function getSkeleton() {
  return requireBridge().getSkeleton();
}
function freeCharacter(_character) {
}
export {
  freeCharacter,
  generate,
  getLastError,
  getSkeleton,
  init
};
