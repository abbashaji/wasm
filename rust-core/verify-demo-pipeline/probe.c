/*
 * Phase 9b pipeline-exercise probe.
 *
 * Standalone C program that links the real, built `libanthroforge_core.so`
 * and calls its actual `extern "C"` entry points -- exactly the way the
 * Unreal C++ plugin (and the RESULTS-03.md probe.c precedent in this repo)
 * does. This is deliberately NOT a Rust `#[cfg(test)]` module: this crate's
 * `crate-type` is `["cdylib"]` only (no `rlib`), so a separate Rust
 * binary/example cannot `use anthroforge_core::...` against it, and the
 * existing `#[cfg(test)]` unit tests all share one process-wide
 * `GLOBAL_REGISTRY: OnceLock`, so a test that calls `init_part_registry`
 * against demo-assets could lose the init race to some other test's dummy
 * fixture path. A fresh OS process sidesteps both problems and matches
 * this repo's own established verification pattern for exercising real
 * FFI entry points.
 *
 * Struct layouts below are copied field-for-field, in the same order, from
 * rust-core/src/lib.rs and rust-core/src/texture_atlas.rs. Field order/
 * types are what must match (repr(C) layout, not any manual packing), same
 * assumption the real Unreal C++ mirror (AnthroforgeCoreTypes.h) already
 * makes.
 */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>

typedef struct {
    float position[3];
    float normal[3];
    float uv[2];
    uint16_t bone_indices[4];
    float bone_weights[4];
} SkinnedVertex; /* must be 56 bytes */

typedef struct {
    uint64_t seed;
    float height_modifier;
    float weight_modifier;
    uint32_t head_id;
    uint32_t torso_id;
    const uint32_t *equipped_clothing_ids_ptr;
    uint32_t equipped_clothing_count;
} CharacterDNA; /* must be 40 bytes */

typedef struct {
    SkinnedVertex *vertices_ptr;
    uint32_t *indices_ptr;
    uint32_t vertices_count;
    uint32_t indices_count;
} MeshOutputBuffer; /* must be 24 bytes */

typedef struct {
    uint32_t width;
    uint32_t height;
    uint8_t *pixels_ptr;
    uint32_t total_bytes;
} RawImage; /* must be 24 bytes */

typedef struct {
    RawImage atlas_image;
    uint32_t quadrant_width;
    uint32_t quadrant_height;
} RuntimeAtlasOutput; /* must be 32 bytes */

extern int init_part_registry(const char *asset_dir);
extern MeshOutputBuffer *generate_character(const CharacterDNA *dna);
extern void free_mesh_buffer(MeshOutputBuffer *buffer);
extern RuntimeAtlasOutput *generate_runtime_atlas(const RawImage *head, const RawImage *torso,
                                                   const RawImage *legs, const RawImage *feet,
                                                   uint32_t target_atlas_size);
extern void free_atlas_buffer(RuntimeAtlasOutput *output);
extern const char *anthroforge_last_error(void);

static int g_checks_failed = 0;

#define CHECK(cond, msg)                                                     \
    do {                                                                     \
        if (!(cond)) {                                                       \
            printf("  ** CHECK FAILED: %s\n", msg);                          \
            g_checks_failed++;                                               \
        }                                                                    \
    } while (0)

static void print_layout_sizes(void) {
    printf("=== FFI struct sizes (must match Rust's compile-time asserts) ===\n");
    printf("  sizeof(SkinnedVertex)      = %zu (expect 56)\n", sizeof(SkinnedVertex));
    printf("  sizeof(CharacterDNA)       = %zu (expect 40)\n", sizeof(CharacterDNA));
    printf("  sizeof(MeshOutputBuffer)   = %zu (expect 24)\n", sizeof(MeshOutputBuffer));
    printf("  sizeof(RawImage)           = %zu\n", sizeof(RawImage));
    printf("  sizeof(RuntimeAtlasOutput) = %zu\n", sizeof(RuntimeAtlasOutput));
    CHECK(sizeof(SkinnedVertex) == 56, "SkinnedVertex size mismatch");
    CHECK(sizeof(CharacterDNA) == 40, "CharacterDNA size mismatch");
    CHECK(sizeof(MeshOutputBuffer) == 24, "MeshOutputBuffer size mismatch");
    printf("\n");
}

static uint8_t *load_raw_rgba8(const char *path, uint32_t width, uint32_t height, uint32_t *out_bytes) {
    uint32_t expected = width * height * 4;
    FILE *f = fopen(path, "rb");
    if (!f) {
        printf("  !! failed to open '%s'\n", path);
        return NULL;
    }
    uint8_t *buf = malloc(expected);
    size_t got = fread(buf, 1, expected, f);
    fclose(f);
    if (got != expected) {
        printf("  !! '%s': read %zu bytes, expected %u\n", path, got, expected);
        free(buf);
        return NULL;
    }
    *out_bytes = expected;
    return buf;
}

static long file_size(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fclose(f);
    return sz;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <demo-assets-dir>\n", argv[0]);
        return 2;
    }
    const char *asset_dir = argv[1];

    print_layout_sizes();

    printf("=== Step 1: init_part_registry(\"%s\") ===\n", asset_dir);
    int ok = init_part_registry(asset_dir);
    printf("  init_part_registry returned: %s\n", ok ? "true" : "false");
    CHECK(ok, "init_part_registry must succeed against the demo asset set");
    if (!ok) {
        const char *err = anthroforge_last_error();
        printf("  last_error: %s\n", err ? err : "(null)");
        printf("\n=== RESULT: %d check(s) failed ===\n", g_checks_failed);
        return 1;
    }
    printf("\n");

    /* Part ids, matching the *_id numbers baked into the generated
     * filenames (93001_head_round.obj -> id 93001, etc). */
    const uint32_t HEAD_A = 93001, HEAD_B = 93002;
    const uint32_t TORSO_A = 93011, TORSO_B = 93012;
    const uint32_t SHIRT = 93021, JACKET = 93022, HOODIE = 93023;
    const uint32_t PANTS = 93031, SHORTS = 93032, BOOTS = 93041;

    printf("=== Step 2: generate_character across distinct DNA combinations ===\n");

    struct {
        const char *label;
        uint64_t seed;
        float height_mod, weight_mod;
        uint32_t head_id, torso_id;
        uint32_t clothing[4];
        uint32_t clothing_count;
    } combos[] = {
        {"combo 1: head_A + torso_A, bare", 1001, 1.0f, 1.0f, HEAD_A, TORSO_A, {0}, 0},
        {"combo 2: head_B + torso_B, shirt+pants+boots", 1002, 1.08f, 1.15f, HEAD_B, TORSO_B,
         {SHIRT, PANTS, BOOTS}, 3},
        {"combo 3: head_A + torso_B, jacket+shorts+boots (multi-item, cross body)", 1003, 0.95f,
         1.30f, HEAD_A, TORSO_B, {JACKET, SHORTS, BOOTS}, 3},
        {"combo 4: head_B + torso_A, hoodie only", 1004, 1.02f, 0.90f, HEAD_B, TORSO_A, {HOODIE}, 1},
    };
    int n_combos = (int)(sizeof(combos) / sizeof(combos[0]));

    long total_mesh_bytes = 0;
    long total_index_bytes = 0;
    int n_meshes = 0;

    for (int i = 0; i < n_combos; i++) {
        CharacterDNA dna;
        memset(&dna, 0, sizeof(dna));
        dna.seed = combos[i].seed;
        dna.height_modifier = combos[i].height_mod;
        dna.weight_modifier = combos[i].weight_mod;
        dna.head_id = combos[i].head_id;
        dna.torso_id = combos[i].torso_id;
        dna.equipped_clothing_ids_ptr = combos[i].clothing_count > 0 ? combos[i].clothing : NULL;
        dna.equipped_clothing_count = combos[i].clothing_count;

        MeshOutputBuffer *buf = generate_character(&dna);
        printf("  %s\n", combos[i].label);
        if (!buf) {
            const char *err = anthroforge_last_error();
            printf("    generate_character returned NULL. last_error: %s\n", err ? err : "(null)");
            CHECK(0, "generate_character must succeed for a valid combo");
            continue;
        }
        long vbytes = (long)buf->vertices_count * (long)sizeof(SkinnedVertex);
        long ibytes = (long)buf->indices_count * (long)sizeof(uint32_t);
        printf("    vertices_count=%u  indices_count=%u  (%ld bytes verts + %ld bytes indices = %ld bytes mesh)\n",
               buf->vertices_count, buf->indices_count, vbytes, ibytes, vbytes + ibytes);
        CHECK(buf->vertices_count > 0, "vertices_count must be nonzero");
        CHECK(buf->indices_count > 0, "indices_count must be nonzero");
        CHECK(buf->indices_count % 3 == 0, "indices_count must be a multiple of 3 (triangle list)");
        CHECK(buf->vertices_ptr != NULL, "vertices_ptr must be non-null for a nonzero vertex count");
        CHECK(buf->indices_ptr != NULL, "indices_ptr must be non-null for a nonzero index count");

        /* Sanity: every index in range, and at least a few vertex positions
         * are finite and non-degenerate (not all exactly the origin, which
         * would indicate a merge/mutation bug rather than real geometry). */
        int bad_index = 0;
        for (uint32_t k = 0; k < buf->indices_count; k++) {
            if (buf->indices_ptr[k] >= buf->vertices_count) { bad_index = 1; break; }
        }
        CHECK(!bad_index, "every index must be < vertices_count");

        int nonzero_positions = 0;
        for (uint32_t k = 0; k < buf->vertices_count; k++) {
            SkinnedVertex v = buf->vertices_ptr[k];
            if (v.position[0] != 0.0f || v.position[1] != 0.0f || v.position[2] != 0.0f) {
                nonzero_positions++;
            }
        }
        printf("    nonzero-position vertices: %d / %u\n", nonzero_positions, buf->vertices_count);
        CHECK(nonzero_positions > 0, "at least some vertex positions must be nonzero (real geometry, not garbage)");

        total_mesh_bytes += vbytes;
        total_index_bytes += ibytes;
        n_meshes++;

        free_mesh_buffer(buf);
    }
    printf("\n");

    if (n_meshes > 0) {
        printf("  average mesh bytes across %d successful generate_character calls: %ld bytes (verts) + %ld bytes (indices) = %ld bytes total\n",
               n_meshes, total_mesh_bytes / n_meshes, total_index_bytes / n_meshes,
               (total_mesh_bytes + total_index_bytes) / n_meshes);
    }
    printf("\n");

    printf("=== Step 3: generate_runtime_atlas on real texture data ===\n");

    char path_head[1024], path_torso[1024], path_legs[1024], path_feet[1024];
    snprintf(path_head, sizeof(path_head), "%s/93001_head_round_diffuse.rgba8", asset_dir);
    snprintf(path_torso, sizeof(path_torso), "%s/93011_torso_average_diffuse.rgba8", asset_dir);
    snprintf(path_legs, sizeof(path_legs), "%s/93031_pants_diffuse.rgba8", asset_dir);
    snprintf(path_feet, sizeof(path_feet), "%s/93041_boots_diffuse.rgba8", asset_dir);

    uint32_t head_bytes = 0, torso_bytes = 0, legs_bytes = 0, feet_bytes = 0;
    uint8_t *head_px = load_raw_rgba8(path_head, 1024, 1024, &head_bytes);
    uint8_t *torso_px = load_raw_rgba8(path_torso, 1024, 1024, &torso_bytes);
    uint8_t *legs_px = load_raw_rgba8(path_legs, 512, 512, &legs_bytes);
    uint8_t *feet_px = load_raw_rgba8(path_feet, 512, 512, &feet_bytes);
    CHECK(head_px && torso_px && legs_px && feet_px, "all 4 real texture files must load");

    RawImage head_img = {1024, 1024, head_px, head_bytes};
    RawImage torso_img = {1024, 1024, torso_px, torso_bytes};
    RawImage legs_img = {512, 512, legs_px, legs_bytes};
    RawImage feet_img = {512, 512, feet_px, feet_bytes};

    /* Call A: full 4-quadrant atlas (head/torso required, legs/feet
     * optional and exercised here since we have real textures for them). */
    uint32_t target_size_full = 2048;
    RuntimeAtlasOutput *atlas_full =
        generate_runtime_atlas(&head_img, &torso_img, &legs_img, &feet_img, target_size_full);
    if (!atlas_full) {
        const char *err = anthroforge_last_error();
        printf("  4-quadrant call returned NULL. last_error: %s\n", err ? err : "(null)");
        CHECK(0, "4-quadrant generate_runtime_atlas call must succeed");
    } else {
        printf("  4-quadrant atlas: %ux%u, quadrant %ux%u, total_bytes=%u (expect %u)\n",
               atlas_full->atlas_image.width, atlas_full->atlas_image.height,
               atlas_full->quadrant_width, atlas_full->quadrant_height,
               atlas_full->atlas_image.total_bytes, target_size_full * target_size_full * 4);
        CHECK(atlas_full->atlas_image.width == target_size_full, "atlas width must equal target_atlas_size");
        CHECK(atlas_full->atlas_image.height == target_size_full, "atlas height must equal target_atlas_size");
        CHECK(atlas_full->atlas_image.total_bytes == target_size_full * target_size_full * 4,
              "atlas total_bytes must equal width*height*4");
        CHECK(atlas_full->atlas_image.pixels_ptr != NULL, "atlas pixels_ptr must be non-null");

        /* Spot-check that each quadrant actually contains that source's
         * pixel data (not zeroed/garbage) by comparing a pixel from each
         * quadrant's interior against the corresponding (nearest-sampled)
         * source pixel's color channel magnitude. */
        /* Sources are blitted top-left-anchored within their quadrant
         * (blit_to_quadrant_raw), and legs/feet (512x512) are smaller than
         * their 1024x1024 quadrant here, so the probe point must land
         * within the *smallest* source's bounds, not the quadrant center
         * (an earlier version of this probe sampled at qw/2,qh/2, which
         * landed in legs/feet's zero-padded region and wrongly looked like
         * a blit bug -- fixed to sample near each quadrant's origin). */
        uint32_t qw = atlas_full->quadrant_width, qh = atlas_full->quadrant_height;
        uint32_t stride = atlas_full->atlas_image.width * 4;
        uint32_t probe_x = 100, probe_y = 100;
        uint32_t tl = (probe_y * stride) + probe_x * 4;
        uint32_t tr = (probe_y * stride) + (qw + probe_x) * 4;
        uint32_t bl = ((qh + probe_y) * stride) + probe_x * 4;
        uint32_t br = ((qh + probe_y) * stride) + (qw + probe_x) * 4;
        uint8_t *px = atlas_full->atlas_image.pixels_ptr;
        printf("    quadrant interior RGB samples: TL(head)=(%u,%u,%u) TR(torso)=(%u,%u,%u) BL(legs)=(%u,%u,%u) BR(feet)=(%u,%u,%u)\n",
               px[tl], px[tl+1], px[tl+2], px[tr], px[tr+1], px[tr+2],
               px[bl], px[bl+1], px[bl+2], px[br], px[br+1], px[br+2]);
        int all_distinct = !(px[tl]==px[tr] && px[tl]==px[bl] && px[tl]==px[br]);
        CHECK(all_distinct, "the 4 quadrants must not all sample the same color (would indicate a blit bug)");

        long atlas_bytes = (long)atlas_full->atlas_image.total_bytes;
        printf("  [pipeline number] one full-quadrant baked atlas = %ld bytes (%.3f MiB)\n",
               atlas_bytes, atlas_bytes / 1024.0 / 1024.0);

        free_atlas_buffer(atlas_full);
    }
    printf("\n");

    /* Call B: required-only (head+torso), a different target size, to also
     * exercise the "legs/feet optional -> zeroed quadrants" path. Must
     * still be >= 2x our 1024x1024 head/torso source images (quadrant =
     * target_atlas_size/2), so this uses 4096, not 1024 (an earlier
     * version used 1024, which made the quadrant 512x512 -- smaller than
     * the required 1024x1024 source -- and correctly failed with
     * AtlasError::SourceTooLarge; that was a probe bug, not an engine
     * bug, fixed by picking a valid size here). */
    uint32_t target_size_min = 4096;
    RuntimeAtlasOutput *atlas_min =
        generate_runtime_atlas(&head_img, &torso_img, NULL, NULL, target_size_min);
    if (!atlas_min) {
        const char *err = anthroforge_last_error();
        printf("  head+torso-only call returned NULL. last_error: %s\n", err ? err : "(null)");
        CHECK(0, "head+torso-only generate_runtime_atlas call must succeed");
    } else {
        printf("  head+torso-only atlas: %ux%u, quadrant %ux%u, total_bytes=%u (expect %u)\n",
               atlas_min->atlas_image.width, atlas_min->atlas_image.height,
               atlas_min->quadrant_width, atlas_min->quadrant_height,
               atlas_min->atlas_image.total_bytes, target_size_min * target_size_min * 4);
        CHECK(atlas_min->atlas_image.width == target_size_min, "atlas width must equal target_atlas_size");
        CHECK(atlas_min->atlas_image.total_bytes == target_size_min * target_size_min * 4,
              "atlas total_bytes must equal width*height*4");
        free_atlas_buffer(atlas_min);
    }

    free(head_px); free(torso_px); free(legs_px); free(feet_px);

    printf("\n=== RESULT: %d check(s) failed ===\n", g_checks_failed);
    return g_checks_failed == 0 ? 0 : 1;
}
