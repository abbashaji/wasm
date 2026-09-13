//! Phase 4 — Runtime Texture Atlasing
//!
//! Combines up to four independently-generated RGBA8 textures (head, torso,
//! legs, feet) into a single fixed-grid atlas so the renderer only needs one
//! texture bind per character instead of four.
//!
//! Safety contract for this module:
//! - No `unwrap()` / `panic!()` / `assert!()` is reachable from an
//!   `extern "C"` entry point. All fallible paths return `Result` internally
//!   and collapse to a null pointer / no-op at the FFI boundary.
//! - Every raw-pointer write is preceded by an explicit bounds check against
//!   both the source image and the destination atlas buffer.
//! - `#[repr(C)]` is applied to every struct that crosses the FFI boundary.

use std::mem::ManuallyDrop;
use std::panic::{self, AssertUnwindSafe};
use std::ptr;

use crate::error::{clear_last_error, set_last_error};

/// Bytes per pixel for RGBA8.
const BYTES_PER_PIXEL: u64 = 4;

// ---------------------------------------------------------------------
// FFI-facing data types
// ---------------------------------------------------------------------

/// A raw RGBA8 image buffer owned by the caller (for inputs) or by this
/// module (for the atlas output, released via `free_atlas_buffer`).
#[repr(C)]
pub struct RawImage {
    pub width: u32,
    pub height: u32,
    /// Pointer to `total_bytes` bytes of tightly-packed RGBA8 data
    /// (row-major, no padding between rows).
    pub pixels_ptr: *mut u8,
    pub total_bytes: u32,
}

// RawImage's only pointer field (`pixels_ptr`) is treated as thread-safe by
// construction in `generate_runtime_atlas_impl`: worker threads are only
// ever handed a `&RawImage` for a *source* (read-only) image, and every
// write into the (separate) atlas buffer goes through the already-Send
// `SendPtr` wrapper below, restricted to a pre-validated, disjoint
// byte range per thread. Needed to build at all: `std::thread::scope`
// requires every captured `&RawImage` to be `Send`/`Sync`, which does not
// hold automatically for a struct containing a raw pointer. Build-only fix,
// not an ABI change — field layout, names, and order are untouched, so this
// does not affect the C++ mirror in AnthroforgeCoreTypes.h.
//
// Scope note (see UNSAFE_AUDIT.md entry #14): this is a *type-level* grant,
// not a call-site-scoped one — it applies to every future use of `RawImage`
// anywhere in this crate, not just the one place it's used today. It's
// sound right now because `RawImage` is used nowhere in this crate except
// read-only, inside `generate_runtime_atlas_impl`'s `std::thread::scope`
// block. If `RawImage` is ever reused elsewhere — especially anywhere it
// might be mutated, or shared across threads outside a scope that's joined
// before returning — this impl needs to be re-audited, or narrowed back to
// a call-site-scoped wrapper instead. (The original Phase-5 rounds used a
// `SendSourceRef<'a>(&'a RawImage)`-style pattern for exactly that purpose;
// that's the fallback if this type-level grant ever stops being safe.)
unsafe impl Send for RawImage {}
unsafe impl Sync for RawImage {}

/// Result of a successful atlas build.
#[repr(C)]
pub struct RuntimeAtlasOutput {
    pub atlas_image: RawImage,
    pub quadrant_width: u32,
    pub quadrant_height: u32,
}

// ---------------------------------------------------------------------
// Internal (non-FFI) types
// ---------------------------------------------------------------------

/// Fixed grid position within the atlas. Values are intentionally explicit
/// (not derived from array index) so a bad cast can't silently reorder
/// quadrants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quadrant {
    /// Head
    TopLeft,
    /// Torso
    TopRight,
    /// Legs
    BottomLeft,
    /// Feet
    BottomRight,
}

impl Quadrant {
    /// Pixel-space origin (top-left corner) of this quadrant within the atlas.
    fn origin(self, quadrant_width: u32, quadrant_height: u32) -> (u32, u32) {
        match self {
            Quadrant::TopLeft => (0, 0),
            Quadrant::TopRight => (quadrant_width, 0),
            Quadrant::BottomLeft => (0, quadrant_height),
            Quadrant::BottomRight => (quadrant_width, quadrant_height),
        }
    }

    /// Normalized (0.0..=1.0) UV sub-rectangle this quadrant occupies within
    /// the combined atlas.
    fn uv_bounds(self) -> (Uv, Uv) {
        match self {
            Quadrant::TopLeft => (Uv { u: 0.0, v: 0.0 }, Uv { u: 0.5, v: 0.5 }),
            Quadrant::TopRight => (Uv { u: 0.5, v: 0.0 }, Uv { u: 1.0, v: 0.5 }),
            Quadrant::BottomLeft => (Uv { u: 0.0, v: 0.5 }, Uv { u: 0.5, v: 1.0 }),
            Quadrant::BottomRight => (Uv { u: 0.5, v: 0.5 }, Uv { u: 1.0, v: 1.0 }),
        }
    }
}

/// All failure modes for atlas construction. Never allowed to panic; always
/// surfaced as `Err` internally and collapsed to `null` / no-op at the FFI
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtlasError {
    /// A required source image pointer (head/torso) was null.
    NullRequiredSource,
    /// A provided source image had a null pixel buffer.
    NullPixelData,
    /// A source image reported zero width or height.
    InvalidSourceDimensions,
    /// Source image is larger than the quadrant it must fit into.
    SourceTooLarge {
        source_width: u32,
        source_height: u32,
        quadrant_width: u32,
        quadrant_height: u32,
    },
    /// `width * height * 4` for the source does not match `total_bytes`.
    ByteLengthMismatch { expected: u64, actual: u64 },
    /// A dimension computation would overflow.
    DimensionOverflow,
    /// The source buffer, per its own reported length, is too small for a
    /// row this function is about to copy (defense-in-depth; should be
    /// unreachable if the earlier checks passed).
    SourceBufferTooSmall,
    /// The destination atlas buffer is too small for a row this function is
    /// about to write (defense-in-depth; should be unreachable if the
    /// earlier checks passed).
    AtlasBufferTooSmall,
    /// `target_atlas_size` was zero, odd, or too large to allocate safely.
    InvalidAtlasSize,
    /// A blitting thread panicked instead of returning a `Result`.
    ThreadPanicked,
}

impl std::fmt::Display for AtlasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for AtlasError {}

/// A single normalized texture coordinate. `#[repr(C)]` so mesh UV arrays
/// produced by earlier phases can be reinterpreted as `&mut [Uv]` directly.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Uv {
    pub u: f32,
    pub v: f32,
}

// ---------------------------------------------------------------------
// Blitting
// ---------------------------------------------------------------------

/// Safe entry point: copies `source` into the given `quadrant` of
/// `atlas_buffer`, row by row. `atlas_buffer` must be `atlas_width *
/// atlas_height * 4` bytes, row-major RGBA8.
///
/// Returns `Err` (writing nothing) if `source` does not fit inside its
/// quadrant, or if its declared `total_bytes` doesn't match its declared
/// dimensions.
pub fn blit_to_quadrant(
    atlas_buffer: &mut [u8],
    atlas_width: u32,
    atlas_height: u32,
    source: &RawImage,
    quadrant: Quadrant,
) -> Result<(), AtlasError> {
    // SAFETY: `atlas_buffer` is a live, exclusively-borrowed slice, so its
    // pointer + len are a valid description of the writable region for the
    // duration of this call.
    unsafe {
        blit_to_quadrant_raw(
            atlas_buffer.as_mut_ptr(),
            atlas_buffer.len(),
            atlas_width,
            atlas_height,
            source,
            quadrant,
        )
    }
}

/// Raw-pointer core of the blit, used both by the safe wrapper above and by
/// the concurrent blit in `generate_runtime_atlas` (where the atlas buffer
/// is shared across threads via disjoint, pre-validated regions rather than
/// a single `&mut [u8]`).
///
/// # Safety
/// The caller must guarantee:
/// - `atlas_ptr` is valid for reads/writes of `atlas_len` bytes for the
///   duration of this call.
/// - No other thread is concurrently writing to the byte range this
///   quadrant occupies (guaranteed by the caller partitioning quadrants
///   before spawning threads).
unsafe fn blit_to_quadrant_raw(
    atlas_ptr: *mut u8,
    atlas_len: usize,
    atlas_width: u32,
    atlas_height: u32,
    source: &RawImage,
    quadrant: Quadrant,
) -> Result<(), AtlasError> {
    if atlas_width == 0 || atlas_height == 0 {
        return Err(AtlasError::InvalidAtlasSize);
    }

    if source.pixels_ptr.is_null() {
        return Err(AtlasError::NullPixelData);
    }

    if source.width == 0 || source.height == 0 {
        return Err(AtlasError::InvalidSourceDimensions);
    }

    let quadrant_width = atlas_width / 2;
    let quadrant_height = atlas_height / 2;

    // --- Bounds check #1: source must fit inside its quadrant. ---
    // This MUST happen before any write occurs.
    if source.width > quadrant_width || source.height > quadrant_height {
        return Err(AtlasError::SourceTooLarge {
            source_width: source.width,
            source_height: source.height,
            quadrant_width,
            quadrant_height,
        });
    }

    // --- Bounds check #2: declared byte length must match declared dims. ---
    let expected_source_bytes = (source.width as u64)
        .checked_mul(source.height as u64)
        .and_then(|px| px.checked_mul(BYTES_PER_PIXEL))
        .ok_or(AtlasError::DimensionOverflow)?;

    if expected_source_bytes != source.total_bytes as u64 {
        return Err(AtlasError::ByteLengthMismatch {
            expected: expected_source_bytes,
            actual: source.total_bytes as u64,
        });
    }

    let (origin_x, origin_y) = quadrant.origin(quadrant_width, quadrant_height);
    let row_bytes = (source.width as u64) * BYTES_PER_PIXEL;

    // Copy row by row: source and destination have different row strides
    // (source.width vs atlas_width), so a single contiguous memcpy over the
    // 2D block is not possible.
    for row in 0..source.height {
        let src_row_start = (row as u64) * row_bytes;
        let src_row_end = src_row_start + row_bytes;

        // --- Bounds check #3 (per row, defense-in-depth): source read. ---
        if src_row_end > source.total_bytes as u64 {
            return Err(AtlasError::SourceBufferTooSmall);
        }

        let dst_x = origin_x as u64;
        let dst_y = (origin_y as u64) + (row as u64);
        let dst_row_start = (dst_y * atlas_width as u64 + dst_x) * BYTES_PER_PIXEL;
        let dst_row_end = dst_row_start + row_bytes;

        // --- Bounds check #4 (per row, defense-in-depth): atlas write. ---
        if dst_row_end > atlas_len as u64 {
            return Err(AtlasError::AtlasBufferTooSmall);
        }

        // SAFETY:
        // - `src_ptr .. src_ptr + row_bytes` is within `source.pixels_ptr`'s
        //   `total_bytes` region, per the check above.
        // - `dst_ptr .. dst_ptr + row_bytes` is within `atlas_ptr`'s
        //   `atlas_len` region, per the check above, and — by construction
        //   of `Quadrant::origin` plus the width/height bound check at the
        //   top of this function — never overlaps a different quadrant.
        // - Source and destination buffers are distinct allocations, so the
        //   regions cannot overlap each other either.
        unsafe {
            let src_ptr = source.pixels_ptr.add(src_row_start as usize) as *const u8;
            let dst_ptr = atlas_ptr.add(dst_row_start as usize);
            ptr::copy_nonoverlapping(src_ptr, dst_ptr, row_bytes as usize);
        }
    }

    Ok(())
}

// Note: this same thread-safety gap was independently rediscovered and
// fixed again while auditing FFI error propagation (see RESULTS-05.md's
// "Prerequisite fix" section). That round's fix used a `SendConstPtr<T>`
// wrapper around the source pointer; it's intentionally not included here
// since it's redundant with the canonical fix already applied above
// (`unsafe impl Send`/`Sync` directly on `RawImage`) — a plain `&RawImage`
// is already `Send` once `RawImage` itself is, so no additional
// call-site wrapper is needed for the read-only source side.

// ---------------------------------------------------------------------
// Thread-safety helper
// ---------------------------------------------------------------------

/// Wraps a raw pointer so it can be moved into a scoped thread closure.
///
/// This is sound *only* because each thread spawned in
/// `generate_runtime_atlas` is handed a pre-validated, mutually disjoint
/// quadrant of the same buffer (proven by `Quadrant::origin` combined with
/// the width/height bound check every thread performs before writing).
struct SendPtr(*mut u8);
// SAFETY: see struct doc comment — access is partitioned by construction.
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

// Note: an earlier draft of this fix added a separate `SendSourceRef<'a>`
// wrapper around `&RawImage` for the closure below. That's now redundant:
// `RawImage` itself is `unsafe impl Send`/`Sync` (see its definition above),
// so a plain `&RawImage` is already `Send`, and no per-call-site wrapper is
// needed on the read-only source side — only the mutable `atlas_ptr`
// (`*mut u8`) still needs `SendPtr`.

// ---------------------------------------------------------------------
// UV remapping
// ---------------------------------------------------------------------

/// Rewrites `uvs` in place, mapping each UV from "whole original texture"
/// space (0.0..=1.0) into the sub-rectangle of the combined atlas occupied
/// by `quadrant`.
///
/// `UV_new = UV_min + (UV_old * (UV_max - UV_min))`
///
/// **Deferred/unwired status:** this is the function that would make a
/// combined atlas's UVs actually line up with mesh triangles, but it is
/// currently only exercised by its own unit test — it's called by neither
/// `generate_runtime_atlas_impl` nor `generate_character` (in `lib.rs`).
/// See the deferred-status note on `generate_runtime_atlas` above; the two
/// gaps (no image loader, no UV-remap integration) are tracked together
/// and deferred to Phase 9b for the same reason.
pub fn remap_uvs_for_quadrant(uvs: &mut [Uv], quadrant: Quadrant) {
    let (uv_min, uv_max) = quadrant.uv_bounds();
    let range_u = uv_max.u - uv_min.u;
    let range_v = uv_max.v - uv_min.v;

    for uv in uvs.iter_mut() {
        uv.u = uv_min.u + uv.u * range_u;
        uv.v = uv_min.v + uv.v * range_v;
    }
}

// ---------------------------------------------------------------------
// FFI entry points
// ---------------------------------------------------------------------

/// Builds a single atlas from up to four source textures.
///
/// **Deferred/unwired status:** this function (and `remap_uvs_for_quadrant`,
/// below) is implemented, tested, and independently audited as sound in
/// isolation, but is not yet called by `generate_character`'s production
/// path, and not yet fed by any image loader — nothing in this crate today
/// loads texture pixel data from disk into a `RawImage`. On the C++ side,
/// the Unreal plugin resolves this symbol at load time but never calls it
/// (see the comment above the `ResolveExport` calls in
/// `AnthroforgeCharacterAssembler.cpp`). Full integration is intentionally
/// deferred to Phase 9b, once real art assets exist to test against — see
/// `ROAD_TO_MARKET.md`.
///
/// - `head` and `torso` are **required**: a null pointer for either causes
///   this function to return `null`.
/// - `legs` and `feet` are **optional**: pass `null` to leave that quadrant
///   zeroed (fully transparent, since alpha channel byte is `0`).
/// - `target_atlas_size` must be a nonzero, even value (e.g. `2048`); it is
///   both the atlas width and height.
///
/// Returns a heap-allocated `*mut RuntimeAtlasOutput` on success, which the
/// caller **must** release via `free_atlas_buffer`. Returns `null` on any
/// validation failure, allocation failure, or internal panic — never
/// writes or returns partially-corrupted data.
///
/// # Safety
/// `head`, `torso`, `legs`, `feet` must each be either null or a valid
/// pointer to a fully-initialized `RawImage` whose `pixels_ptr` is valid
/// for reads of `total_bytes` bytes for the duration of this call.
#[no_mangle]
pub extern "C" fn generate_runtime_atlas(
    head: *const RawImage,
    torso: *const RawImage,
    legs: *const RawImage,
    feet: *const RawImage,
    target_atlas_size: u32,
) -> *mut RuntimeAtlasOutput {
    // Cleared unconditionally at entry — see `init_part_registry`'s
    // matching comment in lib.rs for why.
    clear_last_error();

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        generate_runtime_atlas_impl(head, torso, legs, feet, target_atlas_size)
    }));

    match result {
        Ok(Ok(output)) => Box::into_raw(Box::new(output)),
        Ok(Err(atlas_error)) => {
            set_last_error(format!("generate_runtime_atlas failed: {atlas_error}"));
            ptr::null_mut()
        }
        // A panic anywhere in the impl (including a joined worker thread's
        // propagated panic) must never cross the FFI boundary. Recover
        // whatever message the panic carried (best-effort — panics can
        // carry arbitrary payloads) so it isn't lost entirely; this
        // `downcast_ref` can never itself panic.
        Err(panic_payload) => {
            let panic_message = panic_payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<no panic message available>".to_string());
            set_last_error(format!(
                "generate_runtime_atlas: internal panic: {panic_message}"
            ));
            ptr::null_mut()
        }
    }
}

fn generate_runtime_atlas_impl(
    head: *const RawImage,
    torso: *const RawImage,
    legs: *const RawImage,
    feet: *const RawImage,
    target_atlas_size: u32,
) -> Result<RuntimeAtlasOutput, AtlasError> {
    // Test-only panic trigger (`cargo test` builds only — never compiled
    // into a shipped/release artifact). Sentinel value can never collide
    // with a real `target_atlas_size`. Exists purely so
    // `generate_runtime_atlas_panic_is_caught_not_propagated` below can
    // exercise the real `catch_unwind` path through the real `extern "C"`
    // entry point, as a regression guard for the Test 03 finding: if a
    // future change removes `catch_unwind` from `generate_runtime_atlas`
    // (or otherwise breaks it), this test starts panicking through the
    // FFI boundary. Note this only guards the `catch_unwind` code itself —
    // it can't detect a regression back to `panic = "abort"` in
    // `Cargo.toml`, since `cargo test` never builds with
    // `[profile.release]` settings. See that file's comment.
    #[cfg(test)]
    if target_atlas_size == 0xDEAD_BEEF {
        panic!("test-only: simulated internal panic inside generate_runtime_atlas_impl");
    }

    if target_atlas_size == 0 || target_atlas_size % 2 != 0 {
        return Err(AtlasError::InvalidAtlasSize);
    }

    let quadrant_width = target_atlas_size / 2;
    let quadrant_height = target_atlas_size / 2;

    let total_bytes: u64 = (target_atlas_size as u64)
        .checked_mul(target_atlas_size as u64)
        .and_then(|px| px.checked_mul(BYTES_PER_PIXEL))
        .ok_or(AtlasError::InvalidAtlasSize)?;

    // Keep the output's `total_bytes: u32` representable, and keep the
    // allocation within a sane bound (also guards against pathological
    // `target_atlas_size` values from a hostile/buggy caller).
    if total_bytes > u32::MAX as u64 {
        return Err(AtlasError::InvalidAtlasSize);
    }

    // SAFETY: caller contract on `generate_runtime_atlas` guarantees these
    // pointers are either null or valid `RawImage` pointers.
    let head_ref = unsafe { head.as_ref() };
    let torso_ref = unsafe { torso.as_ref() };
    let legs_ref = unsafe { legs.as_ref() };
    let feet_ref = unsafe { feet.as_ref() };

    let head_ref = head_ref.ok_or(AtlasError::NullRequiredSource)?;
    let torso_ref = torso_ref.ok_or(AtlasError::NullRequiredSource)?;

    // Collect only the quadrants that were actually provided.
    let mut jobs: Vec<(Quadrant, &RawImage)> = Vec::with_capacity(4);
    jobs.push((Quadrant::TopLeft, head_ref));
    jobs.push((Quadrant::TopRight, torso_ref));
    if let Some(legs_ref) = legs_ref {
        jobs.push((Quadrant::BottomLeft, legs_ref));
    }
    if let Some(feet_ref) = feet_ref {
        jobs.push((Quadrant::BottomRight, feet_ref));
    }

    // Zero-initialized: any un-provided quadrant (legs/feet) stays fully
    // transparent RGBA (0,0,0,0) rather than leaking uninitialized memory.
    let mut atlas_buffer = vec![0u8; total_bytes as usize];
    let atlas_len = atlas_buffer.len();
    let atlas_ptr = SendPtr(atlas_buffer.as_mut_ptr());

    // Blit all provided (disjoint) quadrants concurrently. Each thread only
    // ever touches the byte range belonging to its own quadrant, which is
    // proven disjoint from every other quadrant by `Quadrant::origin`.
    #[cfg(not(target_arch = "wasm32"))]
    let results: Vec<Result<(), AtlasError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|(quadrant, source)| {
                let quadrant = *quadrant;
                let source: &RawImage = source;
                let atlas_ptr_for_thread = SendPtr(atlas_ptr.0);
                scope.spawn(move || {
                    // Force capture of the *whole* `SendPtr`, not just its
                    // `.0` field. Rust 2021's disjoint (RFC 2229) closure
                    // capture would otherwise capture the bare `*mut u8`
                    // field directly (since that's all the closure body
                    // below actually reads), which bypasses `SendPtr`'s
                    // `unsafe impl Send` entirely and makes the closure
                    // itself not `Send` — needed to build at all, not an
                    // ABI change.
                    let atlas_ptr_for_thread = atlas_ptr_for_thread;

                    // SAFETY: see `blit_to_quadrant_raw` safety comment;
                    // this thread owns exclusive access to its quadrant's
                    // byte range for the duration of the scope.
                    unsafe {
                        blit_to_quadrant_raw(
                            atlas_ptr_for_thread.0,
                            atlas_len,
                            target_atlas_size,
                            target_atlas_size,
                            source,
                            quadrant,
                        )
                    }
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(inner_result) => inner_result,
                Err(_) => Err(AtlasError::ThreadPanicked),
            })
            .collect()
    });

    // wasm32-unknown-unknown has no native OS thread support under this
    // build (stable toolchain, no `atomics`/`bulk-memory` target
    // features) — `std::thread::scope`'s internal thread spawn would
    // panic, and that panic traps the whole module rather than being
    // recoverable (see this crate's AnthroForge/Web spec §9.4: `panic =
    // "unwind"` does not actually unwind on this target). This produces
    // the exact same `Vec<Result<(), AtlasError>>` shape as the
    // multi-threaded path above by blitting each quadrant sequentially
    // on the calling "thread" (there is only one, on this target)
    // instead of spawning one thread per quadrant.
    #[cfg(target_arch = "wasm32")]
    let results: Vec<Result<(), AtlasError>> = jobs
        .iter()
        .map(|(quadrant, source)| {
            let quadrant = *quadrant;
            let source: &RawImage = source;
            // SAFETY: same disjoint-byte-range argument as the native
            // per-thread call this replaces (see `blit_to_quadrant_raw`'s
            // own safety comment and `Quadrant::origin`), trivially
            // satisfied here since only one "thread" (this one) ever
            // touches `atlas_buffer`, sequentially, one quadrant at a
            // time.
            unsafe {
                blit_to_quadrant_raw(
                    atlas_ptr.0,
                    atlas_len,
                    target_atlas_size,
                    target_atlas_size,
                    source,
                    quadrant,
                )
            }
        })
        .collect();

    if let Some(err) = results.into_iter().find_map(|r| r.err()) {
        // `atlas_buffer` drops normally here — nothing was leaked or handed
        // to the caller.
        return Err(err);
    }

    // Ensure capacity == len so `free_atlas_buffer` can reconstruct the Vec
    // exactly via `Vec::from_raw_parts`.
    atlas_buffer.shrink_to_fit();
    let mut owned = ManuallyDrop::new(atlas_buffer);
    let pixels_ptr = owned.as_mut_ptr();

    Ok(RuntimeAtlasOutput {
        atlas_image: RawImage {
            width: target_atlas_size,
            height: target_atlas_size,
            pixels_ptr,
            total_bytes: total_bytes as u32,
        },
        quadrant_width,
        quadrant_height,
    })
}

/// Releases an atlas produced by `generate_runtime_atlas`. Safe to call
/// with `null` (no-op). Must not be called twice on the same pointer, and
/// the pointer must not be used again after this call.
///
/// # Safety
/// `output` must be either null or a pointer previously returned by
/// `generate_runtime_atlas` that has not already been freed.
#[no_mangle]
pub extern "C" fn free_atlas_buffer(output: *mut RuntimeAtlasOutput) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        if output.is_null() {
            return;
        }

        // SAFETY: caller contract guarantees `output` was produced by
        // `generate_runtime_atlas` and not yet freed, so reconstructing the
        // `Box` here is valid and takes ownership back from the caller.
        let boxed = unsafe { Box::from_raw(output) };
        let RuntimeAtlasOutput { atlas_image, .. } = *boxed;

        if !atlas_image.pixels_ptr.is_null() && atlas_image.total_bytes > 0 {
            // SAFETY: `generate_runtime_atlas_impl` allocated this buffer
            // via `Vec<u8>` with `shrink_to_fit()` called immediately
            // before leaking it, so `len == capacity == total_bytes`,
            // matching what `Vec::from_raw_parts` requires.
            let reclaimed = unsafe {
                Vec::from_raw_parts(
                    atlas_image.pixels_ptr,
                    atlas_image.total_bytes as usize,
                    atlas_image.total_bytes as usize,
                )
            };
            drop(reclaimed);
        }
    }));

    if let Err(panic_payload) = result {
        // A panic must never propagate across the FFI boundary. This
        // signature has no return value to carry a failure sentinel, but
        // now that `anthroforge_last_error()` exists, a caller that wants
        // to know *why* cleanup failed (rather than just that it silently
        // did nothing) can check it, instead of the message only ever
        // reaching stderr.
        let panic_message = panic_payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic_payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<no panic message available>".to_string());
        eprintln!("free_atlas_buffer: internal panic suppressed at FFI boundary: {panic_message}");
        set_last_error(format!(
            "free_atlas_buffer: internal panic suppressed at FFI boundary: {panic_message}"
        ));
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_image(width: u32, height: u32, fill: u8) -> (RawImage, Vec<u8>) {
        let mut pixels = vec![fill; (width * height * 4) as usize];
        let image = RawImage {
            width,
            height,
            pixels_ptr: pixels.as_mut_ptr(),
            total_bytes: pixels.len() as u32,
        };
        (image, pixels)
    }

    #[test]
    fn blit_places_pixels_in_correct_quadrant() {
        let (head, _head_backing) = make_image(64, 64, 0xAA);
        let mut atlas = vec![0u8; 128 * 128 * 4];

        blit_to_quadrant(&mut atlas, 128, 128, &head, Quadrant::TopRight).unwrap();

        // Top-right quadrant origin is (64, 0).
        let px_idx = (0u64 * 128 + 64) * 4;
        assert_eq!(atlas[px_idx as usize], 0xAA);
        // Top-left quadrant must remain untouched.
        assert_eq!(atlas[0], 0);
    }

    #[test]
    fn blit_rejects_oversized_source() {
        let (oversized, _backing) = make_image(200, 64, 0xFF);
        let mut atlas = vec![0u8; 128 * 128 * 4];

        let err = blit_to_quadrant(&mut atlas, 128, 128, &oversized, Quadrant::TopLeft)
            .expect_err("oversized source must be rejected");

        assert!(matches!(err, AtlasError::SourceTooLarge { .. }));
        // Nothing should have been written.
        assert!(atlas.iter().all(|&b| b == 0));
    }

    #[test]
    fn blit_rejects_byte_length_mismatch() {
        let (mut bad, _backing) = make_image(32, 32, 0x11);
        bad.total_bytes -= 4; // lie about the length

        let mut atlas = vec![0u8; 128 * 128 * 4];
        let err = blit_to_quadrant(&mut atlas, 128, 128, &bad, Quadrant::BottomLeft)
            .expect_err("mismatched byte length must be rejected");

        assert!(matches!(err, AtlasError::ByteLengthMismatch { .. }));
    }

    #[test]
    fn full_pipeline_generates_and_frees_atlas() {
        let (head, _b1) = make_image(512, 512, 1);
        let (torso, _b2) = make_image(512, 512, 2);
        let (legs, _b3) = make_image(512, 512, 3);
        let (feet, _b4) = make_image(512, 512, 4);

        let output_ptr = generate_runtime_atlas(&head, &torso, &legs, &feet, 1024);
        assert!(!output_ptr.is_null());

        // SAFETY: `output_ptr` was just returned by `generate_runtime_atlas`
        // above and has not been freed yet.
        unsafe {
            let output = &*output_ptr;
            assert_eq!(output.quadrant_width, 512);
            assert_eq!(output.atlas_image.width, 1024);
            assert_eq!(output.atlas_image.total_bytes, 1024 * 1024 * 4);
        }

        free_atlas_buffer(output_ptr);
    }

    fn last_error_message() -> Option<String> {
        let ptr = crate::anthroforge_last_error();
        if ptr.is_null() {
            None
        } else {
            // SAFETY: non-null return from `anthroforge_last_error` is a
            // valid, NUL-terminated C string for as long as this thread
            // makes no further library call; we copy it out immediately.
            Some(
                unsafe { std::ffi::CStr::from_ptr(ptr) }
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    #[test]
    fn atlas_validation_failure_sets_specific_last_error() {
        let (torso, _b) = make_image(512, 512, 2);
        // `head` is required and null here, so this must fail with
        // `AtlasError::NullRequiredSource`.
        let out = generate_runtime_atlas(ptr::null(), &torso, ptr::null(), ptr::null(), 1024);
        assert!(out.is_null());

        let message = last_error_message().expect("validation failure must set a last error");
        assert!(
            message.contains("NullRequiredSource"),
            "expected the specific AtlasError variant in the message, got: {message}"
        );
    }

    #[test]
    fn invalid_atlas_size_sets_specific_last_error() {
        let (head, _b1) = make_image(64, 64, 1);
        let (torso, _b2) = make_image(64, 64, 2);
        // Odd target size is rejected by `InvalidAtlasSize`.
        let out = generate_runtime_atlas(&head, &torso, ptr::null(), ptr::null(), 3);
        assert!(out.is_null());

        let message = last_error_message().expect("validation failure must set a last error");
        assert!(message.contains("InvalidAtlasSize"), "got: {message}");
    }

    #[test]
    fn successful_atlas_generation_clears_prior_last_error() {
        // First induce a failure so a stale message exists.
        let out = generate_runtime_atlas(ptr::null(), ptr::null(), ptr::null(), ptr::null(), 1024);
        assert!(out.is_null());
        assert!(last_error_message().is_some());

        // Then a real success must clear it.
        let (head, _b1) = make_image(512, 512, 1);
        let (torso, _b2) = make_image(512, 512, 2);
        let output_ptr = generate_runtime_atlas(&head, &torso, ptr::null(), ptr::null(), 1024);
        assert!(!output_ptr.is_null());
        assert!(
            last_error_message().is_none(),
            "a successful call must clear any previously-recorded error"
        );

        free_atlas_buffer(output_ptr);
    }

    #[test]
    fn full_pipeline_all_quadrants_receive_correct_distinct_data() {
        // Regression test for the disjoint-closure-capture Send bypass
        // fixed on `RawImage`/`SendPtr` above. `full_pipeline_generates_and_frees_atlas`
        // above never inspects per-quadrant pixel content — it only checks
        // dimensions/byte counts — so it would still pass even if every
        // spawned thread silently received the same `source` (e.g. a
        // capture bug binding a stale/shared value instead of each job's
        // own pair) or if quadrant math regressed at a boundary. This test
        // gives each quadrant fully distinct fill data and checks both the
        // interior and the exact seam pixels on either side of each
        // boundary, so it fails on:
        //   - wrong-source-per-thread (closure capture) bugs,
        //   - quadrant-origin math regressions,
        //   - cross-quadrant write corruption from a genuine data race.
        // It intentionally does NOT (and can't, without a sanitizer)
        // directly assert that the blits happened concurrently rather than
        // sequentially — the compiler-rejected `Send` bypass bug is a
        // compile-time defect, not a runtime one, so no test run could
        // observe it directly; the build failing to compile is what
        // catches it. This test's job is to catch runtime consequences of
        // a wrong *fix* to that bug (e.g. one that compiles by accidentally
        // sharing state across threads).
        let atlas_size = 256u32;
        let quadrant_size = atlas_size / 2; // 128, fills each quadrant exactly

        let (head, _b1) = make_image(quadrant_size, quadrant_size, 0x11);
        let (torso, _b2) = make_image(quadrant_size, quadrant_size, 0x22);
        let (legs, _b3) = make_image(quadrant_size, quadrant_size, 0x33);
        let (feet, _b4) = make_image(quadrant_size, quadrant_size, 0x44);

        let output_ptr = generate_runtime_atlas(&head, &torso, &legs, &feet, atlas_size);
        assert!(!output_ptr.is_null());

        // SAFETY: `output_ptr` was just returned by `generate_runtime_atlas`
        // above and has not been freed yet; `pixels_ptr`/`total_bytes` are
        // exactly the pair it produced for the atlas image.
        unsafe {
            let output = &*output_ptr;
            let bytes = std::slice::from_raw_parts(
                output.atlas_image.pixels_ptr,
                output.atlas_image.total_bytes as usize,
            );

            let px = |x: u32, y: u32| -> u8 {
                let idx = (y as u64 * atlas_size as u64 + x as u64) * 4;
                bytes[idx as usize]
            };

            let q = quadrant_size;

            // Interior of each quadrant.
            assert_eq!(px(0, 0), 0x11, "top-left interior should be head data");
            assert_eq!(px(q, 0), 0x22, "top-right interior should be torso data");
            assert_eq!(px(0, q), 0x33, "bottom-left interior should be legs data");
            assert_eq!(px(q, q), 0x44, "bottom-right interior should be feet data");

            // Seam pixels either side of the vertical boundary (x = q) on
            // the top row: must not bleed across quadrants.
            assert_eq!(px(q - 1, 0), 0x11, "last column of head must stay head data");
            assert_eq!(px(q, 0), 0x22, "first column of torso must not be overwritten");

            // Seam pixels either side of the horizontal boundary (y = q)
            // in the left column: must not bleed across quadrants.
            assert_eq!(px(0, q - 1), 0x11, "last row of head must stay head data");
            assert_eq!(px(0, q), 0x33, "first row of legs must not be overwritten");

            // Far corner of each quadrant (exercises the whole quadrant,
            // not just its origin pixel).
            assert_eq!(px(q - 1, q - 1), 0x11, "head's far corner");
            assert_eq!(px(atlas_size - 1, 0), 0x22, "torso's far corner");
            assert_eq!(px(0, atlas_size - 1), 0x33, "legs' far corner");
            assert_eq!(px(atlas_size - 1, atlas_size - 1), 0x44, "feet's far corner");
        }

        free_atlas_buffer(output_ptr);
    }

    #[test]
    fn null_required_source_returns_null() {
        let (torso, _b) = make_image(512, 512, 2);
        let out = generate_runtime_atlas(ptr::null(), &torso, ptr::null(), ptr::null(), 1024);
        assert!(out.is_null());
    }

    #[test]
    fn missing_optional_quadrants_are_zeroed() {
        let (head, _b1) = make_image(512, 512, 9);
        let (torso, _b2) = make_image(512, 512, 9);

        let output_ptr =
            generate_runtime_atlas(&head, &torso, ptr::null(), ptr::null(), 1024);
        assert!(!output_ptr.is_null());

        // SAFETY: `output_ptr` was just returned by `generate_runtime_atlas`
        // above and has not been freed yet; `pixels_ptr`/`total_bytes` are
        // exactly the pair it produced for the atlas image.
        unsafe {
            let output = &*output_ptr;
            let bytes = std::slice::from_raw_parts(
                output.atlas_image.pixels_ptr,
                output.atlas_image.total_bytes as usize,
            );
            // Bottom-right (feet) quadrant origin: (512, 512).
            let idx = (512u64 * 1024 + 512) * 4;
            assert_eq!(bytes[idx as usize], 0);
        }

        free_atlas_buffer(output_ptr);
    }

    #[test]
    fn generate_runtime_atlas_panic_is_caught_not_propagated() {
        // Test 03 regression guard: an internal panic inside
        // `generate_runtime_atlas_impl` must be caught by
        // `generate_runtime_atlas`'s `catch_unwind` and collapse to a
        // clean `null` return, not propagate/abort. This exercises the
        // real, public `extern "C"` entry point end to end — the same
        // code path an external C++ caller goes through.
        let (head, _b1) = make_image(4, 4, 1);
        let (torso, _b2) = make_image(4, 4, 2);

        let out = generate_runtime_atlas(&head, &torso, ptr::null(), ptr::null(), 0xDEAD_BEEF);

        assert!(
            out.is_null(),
            "an internal panic must collapse to a null return via catch_unwind, not propagate"
        );
    }

    #[test]
    fn uv_remap_matches_quadrant_bounds() {
        let mut uvs = [
            Uv { u: 0.0, v: 0.0 },
            Uv { u: 1.0, v: 1.0 },
            Uv { u: 0.5, v: 0.5 },
        ];
        remap_uvs_for_quadrant(&mut uvs, Quadrant::BottomRight);

        assert_eq!(uvs[0], Uv { u: 0.5, v: 0.5 });
        assert_eq!(uvs[1], Uv { u: 1.0, v: 1.0 });
        assert_eq!(uvs[2], Uv { u: 0.75, v: 0.75 });
    }
}
