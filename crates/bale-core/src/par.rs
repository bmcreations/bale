//! Bounded parallelism for per-tree work that waits on I/O rather than CPU
//! (file walks, git processes): `bale ls` and sweep's sizing.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Threads [`par_map`] spreads work over.
pub const THREADS: usize = 8;

/// `f` over `items` on up to [`THREADS`] threads, results in input order.
pub fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let mut out: Vec<(usize, R)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..THREADS.min(items.len()))
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break };
                        done.push((i, f(item)));
                    }
                    done
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("par_map worker panicked"))
            .collect()
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_input_order() {
        let items: Vec<u32> = (0..50).collect();
        assert_eq!(
            par_map(&items, |n| n * 2),
            (0..50).map(|n| n * 2).collect::<Vec<_>>()
        );
    }
}
