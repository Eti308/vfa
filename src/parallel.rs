//! Minimal work-stealing parallel map over indices (std only).

use std::sync::atomic::{AtomicUsize, Ordering};

/// Number of worker threads to use when the caller passes `0`.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// `(0..n).map(f)` evaluated on up to `threads` threads, order preserved.
pub fn par_map<T, F>(n: usize, threads: usize, f: F) -> Vec<T>
where
    T: Send,
    F: Fn(usize) -> T + Sync,
{
    let threads = if threads == 0 { default_threads() } else { threads }.min(n.max(1));
    if threads <= 1 || n <= 1 {
        return (0..n).map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut parts: Vec<Vec<(usize, T)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        local.push((i, f(i)));
                    }
                    local
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("worker panicked")).collect()
    });
    let mut out: Vec<Option<T>> = (0..n).map(|_| None).collect();
    for part in parts.iter_mut() {
        for (i, v) in part.drain(..) {
            out[i] = Some(v);
        }
    }
    out.into_iter().map(|v| v.expect("all indices computed")).collect()
}
