export interface CharacterDNA {
    seed: bigint;
    heightModifier: number;
    weightModifier: number;
    headId: number;
    torsoId: number;
    clothingIds: number[];
}
/**
 * Deviation from the frozen spec: the spec's §5 API declares a single flat
 * `vertices: Float32Array` field with no documented interleaving
 * convention. The wasm side produces an array of 56-byte interleaved
 * `SkinnedVertex` structs (position/normal/uv/bone_indices/bone_weights).
 *
 * `bone_indices` is two `u16`s that must stay intact integers — packing
 * them into a `Float32Array` slot would require reinterpreting their bits
 * as a float, which is lossy and would corrupt real index values above
 * 2^24 nowhere near the ceiling, but more importantly is simply the wrong
 * representation for an index. So this field set is de-interleaved into
 * separate typed arrays instead of keeping the single flat `vertices`
 * field. This is a bug fix to the frozen shape (it was going to silently
 * corrupt bone indices), not a redesign — flagged explicitly here and in
 * the handoff notes so the merge context knows the frozen contract was
 * intentionally changed, and why.
 */
export interface GeneratedCharacter {
    /** 3 floats per vertex: x, y, z. */
    positions: Float32Array;
    /** 3 floats per vertex: x, y, z. */
    normals: Float32Array;
    /** 2 floats per vertex: u, v. */
    uvs: Float32Array;
    /** 4 uint16s per vertex, kept as true integers (not float-reinterpreted). */
    boneIndices: Uint16Array;
    /** 4 floats per vertex. */
    boneWeights: Float32Array;
    indices: Uint32Array;
    /**
     * Not produced by `generate_character` today — that call only returns
     * mesh geometry (`MeshOutputBuffer`). Texture-atlas generation
     * (`generate_runtime_atlas`) is a separate, independently-callable wasm
     * export that this call path does not invoke. Always empty for now;
     * kept in the type because the spec's API surface is frozen and callers
     * should not have this field silently disappear later.
     */
    atlasBytes: Uint8Array;
    /** Always 0 for now — see `atlasBytes`. */
    atlasWidth: number;
    /** Always 0 for now — see `atlasBytes`. */
    atlasHeight: number;
}
export interface InitOptions {
    partPackUrl: string;
    licenseKey: string;
}
export interface SkeletonJoint {
    parentIndex: number;
    translation: [number, number, number];
    rotation: [number, number, number, number];
    scale: [number, number, number];
}
/**
 * Loads the SDK's own bundled wasm module (shipped as a sibling file to
 * this package's built JS — not fetched from a caller-supplied URL) and
 * the caller's Part Pack, then instantiates the module.
 */
export declare function init(options: InitOptions): Promise<void>;
/**
 * Synchronous per the frozen spec: the underlying wasm calls are
 * synchronous once the module is instantiated, so no `Promise` is needed
 * here — only `init()` is async, because it has to fetch two files and
 * instantiate the module.
 */
export declare function generate(dna: CharacterDNA): GeneratedCharacter | null;
export declare function getLastError(): string | null;
/**
 * Returns the fully-assembled global skeleton (`master_skeleton.json`'s
 * bone hierarchy plus every bone's real bind-pose local transform,
 * contributed across every loaded part), or `null` on failure (call
 * `getLastError()` for details). This data is read-only, global, and
 * per-process — unlike `generate()`'s per-call output — so call this once
 * after `init()` succeeds, not once per generated character.
 */
export declare function getSkeleton(): SkeletonJoint[] | null;
/**
 * No-op: `generate()` already copies all mesh data out of wasm linear
 * memory into fresh JS typed arrays and immediately calls
 * `free_mesh_buffer` on the wasm-side buffer before returning. By the time
 * a `GeneratedCharacter` reaches the caller there is nothing left on the
 * wasm side to free. Kept as a real function (not thrown/unimplemented)
 * because the frozen API declares it, and callers should be able to call
 * it unconditionally without it being a trap.
 */
export declare function freeCharacter(_character: GeneratedCharacter): void;
