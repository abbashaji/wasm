/*
 * Phase 9c Part 1 scale-stress probe.
 *
 * Standalone C program, alongside (NOT modifying) `probe.c`, that links
 * the real, built `libanthroforge_core.so` and calls its actual
 * `extern "C"` entry points against Phase 9b's real `sample-assets/
 * demo-assets/` asset set -- same struct-layout/FFI pattern as `probe.c`,
 * reused rather than reinvented (see that file's own header comment for
 * the full rationale on why this has to be a standalone C process rather
 * than a Rust `#[cfg(test)]`/`examples/` binary).
 *
 * What this measures, at each of several character counts:
 *   - total wall-clock time for that batch (clock_gettime(CLOCK_MONOTONIC))
 *   - real peak resident set size via getrusage(RUSAGE_SELF, ...).ru_maxrss
 *     (kilobytes on Linux -- chosen over parsing /proc/self/status's
 *     VmHWM because it needs no file I/O or text parsing to get the same
 *     number; both are the same underlying kernel-tracked high-water
 *     mark). NOTE: ru_maxrss is a *process-lifetime* high-water mark, not
 *     a per-batch value -- it never decreases. Batches are run in
 *     increasing character-count order specifically so each batch's
 *     "after" reading is a meaningful (monotonically-valid) new peak, not
 *     because the reading resets between batches. See PHASE_9C_RESULTS.md
 *     for what this does and doesn't tell you as a result.
 *   - whether every generate_character call in the batch returned non-null
 *
 * Every MeshOutputBuffer is freed via free_mesh_buffer immediately after
 * its counts are read, every batch -- a leak here would directly corrupt
 * the memory measurement this probe exists to produce.
 *
 * Build/run (identical pattern to probe.c):
 *   cargo build --release
 *   gcc -O2 -Wall -o scale_probe verify-demo-pipeline/scale_probe.c \
 *       -L target/release -lanthroforge_core -Wl,-rpath,target/release
 *   ./scale_probe /path/to/sample-assets/demo-assets
 */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <time.h>
#include <sys/resource.h>

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

extern int init_part_registry(const char *asset_dir);
extern MeshOutputBuffer *generate_character(const CharacterDNA *dna);
extern void free_mesh_buffer(MeshOutputBuffer *buffer);
extern const char *anthroforge_last_error(void);

/* Real part ids from sample-assets/demo-assets/ (same ids probe.c uses). */
static const uint32_t HEAD_A = 93001, HEAD_B = 93002;
static const uint32_t TORSO_A = 93011, TORSO_B = 93012;
static const uint32_t SHIRT = 93021, JACKET = 93022, HOODIE = 93023;
static const uint32_t PANTS = 93031, SHORTS = 93032, BOOTS = 93041;

/* 4 distinct (head, torso) body pairs, cycling through both real head
 * variants and both real torso variants (not just one fixed body). */
static const uint32_t BODY_HEAD[4] = {HEAD_A, HEAD_A, HEAD_B, HEAD_B};
static const uint32_t BODY_TORSO[4] = {TORSO_A, TORSO_B, TORSO_A, TORSO_B};

/* 8 distinct equipped-clothing combinations (0-3 items each), drawn from
 * all 6 real clothing items -- same combinatorial spirit as probe.c's 4
 * combos, extended to more variety since this probe calls
 * generate_character far more times and a fixed single combo would only
 * exercise the warm-cache path for one (body, clothing-set) key. */
#define MAX_CLOTHING_PER_COMBO 3
typedef struct {
    uint32_t ids[MAX_CLOTHING_PER_COMBO];
    uint32_t count;
} ClothingCombo;

static const ClothingCombo CLOTHING_COMBOS[8] = {
    {{0, 0, 0}, 0},                 /* bare */
    {{SHIRT, 0, 0}, 1},             /* shirt only */
    {{HOODIE, 0, 0}, 1},            /* hoodie only */
    {{JACKET, SHORTS, BOOTS}, 3},   /* jacket+shorts+boots */
    {{SHIRT, PANTS, BOOTS}, 3},     /* shirt+pants+boots */
    {{PANTS, 0, 0}, 1},             /* pants only */
    {{HOODIE, PANTS, BOOTS}, 3},    /* hoodie+pants+boots */
    {{JACKET, 0, 0}, 1},            /* jacket only */
};

static double now_seconds(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

/* ru_maxrss is kilobytes on Linux (this sandbox's platform; see the
 * task's Linux-specific note -- BSD/macOS report bytes instead, so this
 * value is not portable as-is). */
static long peak_rss_kb(void) {
    struct rusage ru;
    getrusage(RUSAGE_SELF, &ru);
    return ru.ru_maxrss;
}

/* Fills `dna` and the caller-owned `clothing_buf` (must have room for
 * MAX_CLOTHING_PER_COMBO entries) for the i-th generated character,
 * cycling deterministically through all 4 body pairs and all 8 clothing
 * combos so a long run exercises every (body, clothing-set) combination
 * repeatedly rather than hammering just one. */
static void build_dna_for_index(long i, CharacterDNA *dna, uint32_t *clothing_buf) {
    const ClothingCombo *combo = &CLOTHING_COMBOS[i % 8];
    int body_index = (int)((i / 8) % 4); /* advance body pair every 8 chars, one full clothing-combo cycle */

    memset(dna, 0, sizeof(*dna));
    dna->seed = (uint64_t)i;
    /* Small deterministic variation in height/weight, not just id
     * variety -- keeps DNA mutation genuinely different per character,
     * matching how a real varied population would look, rather than
     * every character at the same body_mutation scale. */
    dna->height_modifier = 0.85f + 0.30f * (float)((i * 37) % 100) / 100.0f;
    dna->weight_modifier = 0.85f + 0.30f * (float)((i * 53) % 100) / 100.0f;
    dna->head_id = BODY_HEAD[body_index];
    dna->torso_id = BODY_TORSO[body_index];

    if (combo->count > 0) {
        memcpy(clothing_buf, combo->ids, combo->count * sizeof(uint32_t));
        dna->equipped_clothing_ids_ptr = clothing_buf;
        dna->equipped_clothing_count = combo->count;
    } else {
        dna->equipped_clothing_ids_ptr = NULL;
        dna->equipped_clothing_count = 0;
    }
}

typedef struct {
    long count;
    double wall_seconds;
    long rss_before_kb;
    long rss_after_kb;
    long null_returns;
    long total_vertices;
    long total_indices;
} BatchResult;

static BatchResult run_batch(long count) {
    BatchResult result;
    memset(&result, 0, sizeof(result));
    result.count = count;
    result.rss_before_kb = peak_rss_kb();

    double start = now_seconds();

    for (long i = 0; i < count; i++) {
        CharacterDNA dna;
        uint32_t clothing_buf[MAX_CLOTHING_PER_COMBO];
        build_dna_for_index(i, &dna, clothing_buf);

        MeshOutputBuffer *buf = generate_character(&dna);
        if (!buf) {
            result.null_returns++;
            /* Per the task spec: a null mid-batch is itself a reportable
             * result, not silently averaged over. Print immediately so
             * it's visible in the raw log, then continue the batch. */
            const char *err = anthroforge_last_error();
            printf("    !! generate_character returned NULL at i=%ld (head=%u torso=%u): %s\n",
                   i, dna.head_id, dna.torso_id, err ? err : "(null)");
            continue;
        }

        result.total_vertices += buf->vertices_count;
        result.total_indices += buf->indices_count;

        /* Free immediately after reading counts -- do not hold buffers
         * across the loop, per the task's explicit leak-prevention
         * requirement (a leak here would corrupt the RSS numbers this
         * probe exists to produce). */
        free_mesh_buffer(buf);
    }

    double end = now_seconds();
    result.wall_seconds = end - start;
    result.rss_after_kb = peak_rss_kb();
    return result;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <demo-assets-dir> [extra_count ...]\n", argv[0]);
        return 2;
    }
    const char *asset_dir = argv[1];

    printf("=== Step 0: init_part_registry(\"%s\") ===\n", asset_dir);
    int ok = init_part_registry(asset_dir);
    printf("  init_part_registry returned: %s\n", ok ? "true" : "false");
    if (!ok) {
        const char *err = anthroforge_last_error();
        printf("  last_error: %s\n", err ? err : "(null)");
        return 1;
    }
    printf("\n");

    /* Required minimum set, in increasing order (see the file header
     * comment on why increasing order matters for ru_maxrss's
     * high-water-mark semantics). Extra counts (e.g. 5000) can be passed
     * on the command line and are appended, still processed in the order
     * given, expected to also be increasing. */
    long required_counts[] = {10, 100, 500, 1000};
    int n_required = (int)(sizeof(required_counts) / sizeof(required_counts[0]));

    int n_extra = argc - 2;
    long total_batches = n_required + n_extra;
    long *counts = malloc(sizeof(long) * total_batches);
    for (int i = 0; i < n_required; i++) counts[i] = required_counts[i];
    for (int i = 0; i < n_extra; i++) counts[n_required + i] = atol(argv[2 + i]);

    printf("=== Step 1: scale sweep across %ld character counts ===\n", total_batches);

    BatchResult *results = malloc(sizeof(BatchResult) * total_batches);
    long any_null_total = 0;

    for (long b = 0; b < total_batches; b++) {
        long n = counts[b];
        printf("--- batch: %ld characters ---\n", n);
        BatchResult r = run_batch(n);
        results[b] = r;
        any_null_total += r.null_returns;

        double us_per_char = (r.wall_seconds * 1e6) / (double)n;
        printf("  wall_time: %.6f s total, %.3f us/character (avg)\n", r.wall_seconds, us_per_char);
        printf("  null returns: %ld / %ld\n", r.null_returns, n);
        printf("  peak_rss_kb: before=%ld after=%ld delta=%ld\n",
               r.rss_before_kb, r.rss_after_kb, r.rss_after_kb - r.rss_before_kb);
        if (n - r.null_returns > 0) {
            printf("  avg vertices/char: %.1f  avg indices/char: %.1f\n",
                   (double)r.total_vertices / (double)(n - r.null_returns),
                   (double)r.total_indices / (double)(n - r.null_returns));
        }
        printf("\n");
    }

    printf("=== Summary table ===\n");
    printf("%12s %14s %14s %12s %14s %16s\n",
           "count", "wall_s", "us_per_char", "null_returns", "peak_rss_kb", "rss_delta_kb");
    for (long b = 0; b < total_batches; b++) {
        BatchResult r = results[b];
        double us_per_char = (r.wall_seconds * 1e6) / (double)r.count;
        printf("%12ld %14.6f %14.3f %12ld %14ld %16ld\n",
               r.count, r.wall_seconds, us_per_char, r.null_returns, r.rss_after_kb,
               r.rss_after_kb - r.rss_before_kb);
    }
    printf("\n");

    if (any_null_total > 0) {
        printf("=== RESULT: %ld total NULL generate_character return(s) across all batches ===\n", any_null_total);
    } else {
        printf("=== RESULT: 0 NULL generate_character returns across all batches ===\n");
    }

    free(results);
    free(counts);
    return any_null_total == 0 ? 0 : 1;
}
