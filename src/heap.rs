//! A counting allocator for tests: how many heap bytes a piece of code had
//! alive at its peak, so a test can assert that reading a file doesn't load
//! it whole.
//!
//! It wraps the system allocator and keeps the count **per thread**, because
//! tests run in parallel: a global count would mix in the other tests'
//! allocations. [`peak_during`] measures code that runs on the calling
//! thread.
//!
//! ```ignore
//! let peak = heap::peak_during(|| canon.hash_canonical(rel, file).unwrap());
//! assert!(peak < 1 << 20);
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// The system allocator, plus a per-thread count of live bytes and their peak.
struct Counting;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

thread_local! {
    /// Bytes this thread has allocated and not yet freed. Signed: memory
    /// allocated on one thread may be freed on another.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    /// The highest `LIVE` seen since the last [`peak_during`] started.
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

/// Add `delta` bytes to this thread's live count, raising the peak if needed.
/// `try_with`: during thread teardown the thread-locals may be gone already.
fn record(delta: isize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get() + delta;
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        record(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            record(new_size as isize - layout.size() as isize);
        }
        new
    }
}

/// Run `f` on this thread and return how many heap bytes above the starting
/// level it had alive at its peak (what it allocated and kept at the same
/// time, not the total it allocated).
pub fn peak_during<T>(f: impl FnOnce() -> T) -> usize {
    let start = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let result = f();
    let peak = PEAK.with(Cell::get);
    drop(result);
    (peak - start).max(0) as usize
}
