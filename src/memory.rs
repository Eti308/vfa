//! Heap usage measurement for benchmarks.
//!
//! The `vfa` binary installs [`CountingAlloc`] as its global allocator; the
//! benchmark then reports the peak additional heap use of an operation with
//! [`measure_peak`]. In programs that do not install it (library users, the
//! test harness) all measurements return `None`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Counting wrapper around the system allocator.
pub struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            ACTIVE.store(true, Ordering::Relaxed);
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let now = CURRENT.fetch_add(new_size - layout.size(), Ordering::Relaxed) + new_size - layout.size();
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

/// Run `f` and return its result plus the peak heap bytes allocated on top
/// of what was live before the call (`None` without the counting allocator).
pub fn measure_peak<T>(f: impl FnOnce() -> T) -> (T, Option<usize>) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let v = f();
    let peak = PEAK.load(Ordering::Relaxed);
    (v, ACTIVE.load(Ordering::Relaxed).then(|| peak.saturating_sub(base)))
}
