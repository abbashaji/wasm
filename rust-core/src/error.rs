//! Phase 5 — FFI error propagation.
//!
//! Every `extern "C"` export in this crate collapses failure to a `bool`/
//! null-pointer sentinel (see the Phase 5 write-up's failure-path
//! catalogue), with the specific reason previously visible only on Rust's
//! own `stderr` — invisible to a packaged, non-console Unreal build. This
//! module adds a thread-local "last error" string, retrievable via the new
//! `anthroforge_last_error()` export, following the same pattern `errno`
//! (POSIX) and `GetLastError`/`FormatMessage` (Win32) use.
//!
//! # Design rationale
//! A thread-local last-error string was chosen over an error-code enum
//! returned alongside existing outputs because:
//! - It requires zero changes to any existing function's signature — every
//!   current caller keeps compiling and behaving exactly as before. An
//!   out-param or a return-type change would need every existing call site
//!   in `AnthroforgeCharacterAssembler.cpp` (and any other future C++
//!   caller) to be touched, which the assignment's constraints rule out.
//! - It carries a genuinely specific, human-readable message (the same
//!   detail already computed for the `stderr` print) rather than requiring
//!   a parallel enum + `switch` on the C++ side that would need to be kept
//!   in sync with every Rust-side failure variant across five call sites
//!   in three different modules.
//! - `AssembleCharacterAsync` in the existing C++ is already structured so
//!   that the FFI call and any error-retrieval happen on the same
//!   background thread, immediately adjacent in the same function — the
//!   thread-local model's one real limitation (must be read from the same
//!   thread that made the failing call, before that thread makes another
//!   library call) is not a limitation for this codebase's actual calling
//!   convention.
//!
//! # Lifetime / ownership contract for `anthroforge_last_error()`
//! - The returned pointer is **borrowed**, not owned: it points into
//!   storage this module owns. The caller must **never** `free()` it.
//! - It stays valid only until the *same thread* calls into this library
//!   again (any of `init_part_registry`, `generate_character`,
//!   `generate_runtime_atlas`, or `anthroforge_last_error` itself all may
//!   overwrite or clear it). Callers that need the message to outlive the
//!   next call must copy it out (e.g. `FString(UTF8_TO_TCHAR(ptr))`)
//!   immediately.
//! - It is **not** meaningful across threads: the message is only ever set
//!   on the thread that made the failing call, matching this crate's
//!   existing `generate_character`/`init_part_registry`/
//!   `generate_runtime_atlas` calls, which are synchronous and always
//!   observed for their result on the calling thread.
//! - Returns `null` if no error is currently recorded on this thread
//!   (either nothing has failed yet, or the last call on this thread
//!   succeeded and therefore cleared it).
//!
//! # Panic/UB safety of the mechanism itself
//! - `set_last_error`/`clear_last_error`/`anthroforge_last_error` perform
//!   no allocation-fallible operation that isn't handled: `CString::new`
//!   can only fail on an embedded NUL byte, which is handled by falling
//!   back to a fixed, statically-known-NUL-free message rather than
//!   `unwrap`ing the caller-supplied text.
//! - No unsafe code is required in this module at all.

use std::cell::RefCell;
use std::ffi::{c_char, CString};

thread_local! {
    /// The most recent error recorded on this thread, if any. Cleared at
    /// the start of every fallible `extern "C"` entry point in this crate
    /// (see each export's own call to `clear_last_error()`), so a stale
    /// message from a previous failing call can never leak into a later,
    /// successful one on the same thread.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Records `message` as this thread's last error, replacing (and thus
/// dropping) whatever was recorded before. Never panics: a `message`
/// containing an embedded NUL byte (which would make `CString::new` fail)
/// is replaced with a fixed fallback string instead of unwrapping.
pub(crate) fn set_last_error(message: impl Into<String>) {
    let message = message.into();
    let c_string = CString::new(message).unwrap_or_else(|_| {
        // SAFETY/PANIC-FREEDOM: this literal is a fixed, statically-known
        // string with no interior NUL byte, so `CString::new` on it cannot
        // fail; `.expect` here can never actually panic.
        CString::new("<error message omitted: contained an embedded NUL byte>")
            .expect("static fallback string has no interior NUL byte")
    });
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = Some(c_string);
    });
}

/// Clears this thread's last error, if any. Called at the start of every
/// fallible `extern "C"` entry point (before any of that call's own logic
/// runs) so a subsequent successful call never appears to have failed.
pub(crate) fn clear_last_error() {
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// Returns the most recent error message recorded on the **calling
/// thread**, or `null` if none is currently recorded.
///
/// See this module's doc comment for the full ownership/lifetime contract:
/// the returned pointer is borrowed (never free it), valid only until the
/// next call into this library from the same thread, and thread-local (it
/// only ever reflects errors from calls made on the same thread that
/// calls this function).
///
/// # Safety
/// This function itself is safe to call at any time from any thread; it
/// never dereferences a caller-supplied pointer. The *returned* pointer,
/// however, must be treated as described above: read it (e.g. copy it into
/// an `FString`) before making any further call into this library on the
/// same thread, and never pass it to `free()` or any deallocator.
#[no_mangle]
pub extern "C" fn anthroforge_last_error() -> *const c_char {
    LAST_ERROR.with(|cell| match &*cell.borrow() {
        Some(c_string) => c_string.as_ptr(),
        None => std::ptr::null(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: these tests rely on `LAST_ERROR` being thread-local and each
    // `#[test]` function body running to completion on a single thread
    // (true for both the default `cargo test` runner and `--test-threads=1`),
    // so tests don't need to serialize against each other.

    #[test]
    fn no_error_by_default_returns_null() {
        // A freshly-spawned thread has never called `set_last_error`.
        // (Returns a `bool`, not the raw pointer itself, across the
        // `join()` — a `*const c_char` is not `Send`.)
        let handle = std::thread::spawn(|| anthroforge_last_error().is_null());
        assert!(handle.join().unwrap());
    }

    #[test]
    fn set_then_get_roundtrips_message() {
        clear_last_error();
        set_last_error("something specific went wrong");
        let ptr = anthroforge_last_error();
        assert!(!ptr.is_null());
        // SAFETY: `ptr` was just returned by `anthroforge_last_error` above,
        // on this same thread, with no intervening call into this library,
        // so per that function's contract it is still a valid, NUL-terminated
        // C string.
        let msg = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_str().unwrap();
        assert_eq!(msg, "something specific went wrong");
        clear_last_error();
    }

    #[test]
    fn clear_removes_previous_error() {
        set_last_error("some earlier failure");
        assert!(!anthroforge_last_error().is_null());
        clear_last_error();
        assert!(anthroforge_last_error().is_null());
    }

    #[test]
    fn second_set_overwrites_first_without_leaking_old_message() {
        set_last_error("first failure");
        set_last_error("second failure");
        let ptr = anthroforge_last_error();
        // SAFETY: `ptr` was just returned by `anthroforge_last_error` above,
        // on this same thread, with no intervening call into this library,
        // so per that function's contract it is still a valid, NUL-terminated
        // C string.
        let msg = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_str().unwrap();
        assert_eq!(msg, "second failure");
        clear_last_error();
    }

    #[test]
    fn embedded_nul_byte_does_not_panic() {
        set_last_error("bad \0 message");
        let ptr = anthroforge_last_error();
        assert!(!ptr.is_null());
        // Just confirm it's a valid, readable C string that didn't panic.
        // SAFETY: `ptr` was just returned by `anthroforge_last_error` above,
        // on this same thread, with no intervening call into this library,
        // so per that function's contract it is still a valid, NUL-terminated
        // C string.
        let _ = unsafe { std::ffi::CStr::from_ptr(ptr) };
        clear_last_error();
    }

    #[test]
    fn error_is_thread_local() {
        clear_last_error();
        set_last_error("set on the main test thread");

        // A different thread must not see this thread's error.
        let other_thread_saw_null = std::thread::spawn(|| anthroforge_last_error().is_null())
            .join()
            .unwrap();
        assert!(other_thread_saw_null);

        // This thread's error must still be intact.
        let ptr = anthroforge_last_error();
        assert!(!ptr.is_null());
        clear_last_error();
    }
}
