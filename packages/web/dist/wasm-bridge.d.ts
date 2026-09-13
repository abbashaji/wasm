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
export declare class WasmBridge {
    private readonly exports;
    private constructor();
    static instantiate(wasmBytes: ArrayBuffer | Uint8Array): Promise<WasmBridge>;
    /** Loads a Part Pack (`.afpp`) into the module's part registry. */
    initPartRegistryFromPack(packBytes: Uint8Array): boolean;
    /**
     * Calls `generate_character`, copies the resulting mesh out of wasm linear
     * memory into fresh JS typed arrays, and frees the wasm-side buffer before
     * returning — per the task's memory-ownership requirement, callers never
     * receive a view directly over wasm memory.
     */
    generateCharacter(dna: RawCharacterDNA): RawMesh | null;
    private readMeshOutputBuffer;
    /**
     * Reads and copies out the last error string recorded by the module.
     * Per the ABI contract this must be read before any further call into the
     * module, since the string is only valid until the next call.
     */
    getLastError(): string | null;
}
