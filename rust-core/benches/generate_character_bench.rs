//! Phase 9a — real, runnable performance benchmarks for
//! `anthroforge_core::generate_character` at realistic vertex counts.
//! See `PHASE_9A_TASK_SPEC.md` for the full brief and
//! `PHASE_9A_RESULTS.md` for the actual `cargo bench` output and
//! interpretation.
//!
//! Run with: `cargo bench --features bench` (see `Cargo.toml`'s `bench`
//! feature / `[[bench]]` `required-features` doc comments for why the
//! feature gate exists).
//!
//! # Process-global registry constraint
//! `init_part_registry` can only succeed once per process (see
//! `lib.rs`'s doc comment). Every benchmark function below shares the
//! single `support::bench_assets()` `OnceLock`-backed init instead of
//! attempting its own — this is required, not just an optimization: a
//! second `init_part_registry` call would simply fail and every
//! benchmark after the first would be generating characters against an
//! uninitialized (or a different benchmark's) registry.

mod support;

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use support::{
    bench_assets, call_generate_character, generate_merged_skin_for_kdtree_bench, make_dna,
    FIXED_HEAD_ID, FIXED_TORSO_ID, JACKET_ID, SHIRT_ID, VEST_ID,
};

/// Baseline: mesh-merge + DNA-mutation cost alone, zero clothing-fit path
/// involved at all (`equipped_clothing_count == 0` never touches
/// `Registry::get_or_build_skin_tree` / `get_or_build_clothing_anchors`).
fn bench_generate_character_no_clothing(c: &mut Criterion) {
    let assets = bench_assets();
    let _ = assets; // ensures the shared registry is initialized before timing starts

    let mut group = c.benchmark_group("generate_character");
    group.sample_size(10);
    group.measurement_time(Duration::from_millis(800));
    group.warm_up_time(Duration::from_millis(300));

    group.bench_function("no_clothing", |b| {
        b.iter(|| {
            let dna = make_dna(FIXED_HEAD_ID, FIXED_TORSO_ID, &[]);
            black_box(call_generate_character(black_box(&dna)))
        })
    });

    group.finish();
}

/// First-ever call for a fresh `(head_id, torso_id, clothing_id)` combo:
/// the shared per-body skin KD-tree has never been built for this body
/// before, so this pays the full `mesh_merge` + `build_skin_kdtree` +
/// `build_cloth_anchors_with_tree` cost on top of the baseline. A fresh
/// `(head_id, torso_id)` pair is drawn from `support::BenchAssets`'
/// pre-registered pool on every single call (`next_cold_pair`, an atomic
/// cursor) specifically so criterion's internal calibration/warm-up loop
/// cannot silently turn this into a warm-cache measurement after the
/// first real iteration — every iteration, including calibration ones,
/// hits a genuinely never-before-requested body.
fn bench_generate_character_cold_cache(c: &mut Criterion) {
    let assets = bench_assets();

    let mut group = c.benchmark_group("generate_character");
    group.sample_size(10);
    group.measurement_time(Duration::from_millis(800));
    // Criterion's normal `b.iter(...)` warm-up loop calls the routine
    // repeatedly for `warm_up_time` before any measured sample — fine
    // for the other benchmarks in this file (which all reuse one fixed,
    // already-registered body, so calling them thousands of times is
    // harmless), but wrong here: at this call's actual per-iteration
    // cost (a couple hundred microseconds — see PHASE_9A_RESULTS.md),
    // even a short default warm-up would burn through most of
    // `COLD_CACHE_POOL_SIZE`'s fresh pairs before a single *measured*
    // sample ever ran, exactly the "criterion's normal warm-up loop
    // silently turns this into a warm-cache measurement" failure mode
    // the task spec calls out. Set to (as close to) zero as criterion
    // allows so effectively no fresh pairs are spent on warm-up.
    group.warm_up_time(Duration::from_nanos(1));

    // `iter_custom` instead of plain `b.iter(...)`: we perform exactly
    // *one* real, genuinely-cold `generate_character` call per closure
    // invocation — never more, regardless of the `iters` count criterion
    // internally asks for — and report `elapsed * iters` for that single
    // real call. This is the standard criterion pattern for benchmarking
    // an operation whose realistic input is single-use (a cache key that
    // must be fresh): a plain `b.iter`/`iter_batched` routine is invoked
    // `iters` times per sample and would each need its own fresh pair
    // (thousands, given this call's actual per-iteration cost — quickly
    // exceeding any realistically pre-generatable pool); `iter_custom`
    // lets us decouple "how many fresh pairs we actually consume" (one
    // per sample — `sample_size(10)` above, so 10 total for the whole
    // measured run) from "what duration criterion records" (still the
    // real, individually-measured cold-cache cost, just linearly scaled
    // to satisfy criterion's `Duration-for-iters-calls` contract). This
    // is a legitimate extrapolation specifically *because* every pool
    // body shares byte-identical geometry under a fresh id — the cost
    // being measured is "first-touch a never-before-seen body of this
    // vertex count", which is the same real quantity on every draw.
    group.bench_function("cold_cache_1_clothing_item", |b| {
        b.iter_custom(|iters| {
            let (head_id, torso_id) = assets.next_cold_pair();
            let clothing = [SHIRT_ID];
            let dna = make_dna(head_id, torso_id, &clothing);
            let start = std::time::Instant::now();
            black_box(call_generate_character(black_box(&dna)));
            start.elapsed() * iters as u32
        })
    });

    group.finish();
}

/// Steady-state cost: the fixed `(FIXED_HEAD_ID, FIXED_TORSO_ID,
/// SHIRT_ID)` combo is warmed once (untimed, before `b.iter` starts) so
/// every timed call hits an already-built KD-tree and already-cached
/// anchors — this is the cost real games actually pay on every spawn
/// after the very first one for a given body/clothing combination.
fn bench_generate_character_warm_cache(c: &mut Criterion) {
    let assets = bench_assets();
    let _ = assets;

    // Untimed warm-up: exactly one real call to populate both cache
    // tiers for this fixed body/clothing combo before any measurement
    // starts.
    let warm_clothing = [SHIRT_ID];
    let warm_dna = make_dna(FIXED_HEAD_ID, FIXED_TORSO_ID, &warm_clothing);
    call_generate_character(&warm_dna);

    let mut group = c.benchmark_group("generate_character");
    group.sample_size(10);
    group.measurement_time(Duration::from_millis(800));
    group.warm_up_time(Duration::from_millis(300));

    group.bench_function("warm_cache_1_clothing_item", |b| {
        b.iter(|| {
            let clothing = [SHIRT_ID];
            let dna = make_dna(FIXED_HEAD_ID, FIXED_TORSO_ID, &clothing);
            black_box(call_generate_character(black_box(&dna)))
        })
    });

    group.finish();
}

/// Same warm-cache setup as `bench_generate_character_warm_cache`, but
/// with 3 equipped clothing items on the same body, to see how cost
/// scales with equipped-item count once every relevant cache tier is
/// warm.
fn bench_generate_character_multi_clothing(c: &mut Criterion) {
    let assets = bench_assets();
    let _ = assets;

    // Untimed warm-up for all 3 items before any measurement starts.
    let warm_clothing = [SHIRT_ID, JACKET_ID, VEST_ID];
    let warm_dna = make_dna(FIXED_HEAD_ID, FIXED_TORSO_ID, &warm_clothing);
    call_generate_character(&warm_dna);

    let mut group = c.benchmark_group("generate_character");
    group.sample_size(10);
    group.measurement_time(Duration::from_millis(800));
    group.warm_up_time(Duration::from_millis(300));

    group.bench_function("warm_cache_3_clothing_items", |b| {
        b.iter(|| {
            let clothing = [SHIRT_ID, JACKET_ID, VEST_ID];
            let dna = make_dna(FIXED_HEAD_ID, FIXED_TORSO_ID, &clothing);
            black_box(call_generate_character(black_box(&dna)))
        })
    });

    group.finish();
}

/// Sweeps `SKIN_KDTREE_DECIMATION_STRIDE` over {1, 2, 4, 8, 16} and
/// measures the shared per-body tree's *build* cost alone (not a full
/// `generate_character` call) at each value, via the
/// `#[cfg(feature = "bench")]`-gated `bench_only_build_skin_kdtree`
/// wrapper (see its doc comment in `lib.rs` for why that wrapper exists
/// and exactly what it does and does not change). The shipped default
/// (`generate_character`'s own call path, via
/// `Registry::get_or_build_skin_tree`) always uses the fixed
/// `SKIN_KDTREE_DECIMATION_STRIDE = 4` constant regardless of this sweep.
fn bench_skin_kdtree_decimation_stride(c: &mut Criterion) {
    let merged_skin = generate_merged_skin_for_kdtree_bench();

    let mut group = c.benchmark_group("skin_kdtree_decimation_stride");
    group.sample_size(10);
    group.measurement_time(Duration::from_millis(500));
    group.warm_up_time(Duration::from_millis(200));

    for &stride in &[1usize, 2, 4, 8, 16] {
        group.bench_with_input(BenchmarkId::from_parameter(stride), &stride, |b, &stride| {
            b.iter(|| {
                anthroforge_core::bench_only_build_skin_kdtree(black_box(&merged_skin), stride)
                    .expect("build_skin_kdtree should succeed for valid, finite synthetic geometry")
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_generate_character_no_clothing,
    bench_generate_character_cold_cache,
    bench_generate_character_warm_cache,
    bench_generate_character_multi_clothing,
    bench_skin_kdtree_decimation_stride,
);
criterion_main!(benches);
