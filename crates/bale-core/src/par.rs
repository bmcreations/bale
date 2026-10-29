//! Bounded parallelism for per-tree work that waits on I/O rather than CPU
//! (file walks, git processes): `bale ls` and sweep's sizing. [`ByteBudget`]
//! bounds the memory such work holds at once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

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

/// Caps the bytes that parallel work holds in memory at once. A holder
/// waits until its share fits; one share larger than the whole budget waits
/// for the budget to drain and then runs alone.
pub struct ByteBudget {
    cap: u64,
    used: Mutex<u64>,
    freed: Condvar,
}

/// Returns its bytes to the [`ByteBudget`] on drop.
pub struct BudgetGuard<'a> {
    budget: &'a ByteBudget,
    n: u64,
}

impl ByteBudget {
    pub fn new(cap: u64) -> ByteBudget {
        ByteBudget {
            cap,
            used: Mutex::new(0),
            freed: Condvar::new(),
        }
    }

    /// Blocks until `n` bytes (at most the whole budget) fit, then holds them.
    pub fn acquire(&self, n: u64) -> BudgetGuard<'_> {
        let n = n.min(self.cap);
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        while *used + n > self.cap {
            used = self.freed.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used += n;
        BudgetGuard { budget: self, n }
    }

    /// Bytes currently held.
    pub fn in_use(&self) -> u64 {
        *self.used.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for BudgetGuard<'_> {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        *used -= self.n;
        drop(used);
        self.budget.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_larger_than_budget_still_runs() {
        let budget = ByteBudget::new(1000);
        let g = budget.acquire(5000);
        assert_eq!(budget.in_use(), 1000);
        drop(g);
        assert_eq!(budget.in_use(), 0);
    }

    #[test]
    fn concurrent_holds_never_exceed_budget() {
        use std::sync::atomic::AtomicU64;
        let budget = ByteBudget::new(1000);
        let held = AtomicU64::new(0);
        let peak = AtomicU64::new(0);
        let sizes: Vec<u64> = (0..40).map(|i| [300, 450, 100, 2000][i % 4]).collect();
        par_map(&sizes, |&n| {
            let _g = budget.acquire(n);
            let now = held.fetch_add(n.min(1000), Ordering::SeqCst) + n.min(1000);
            peak.fetch_max(now, Ordering::SeqCst);
            assert!(budget.in_use() <= 1000);
            std::thread::sleep(std::time::Duration::from_millis(2));
            held.fetch_sub(n.min(1000), Ordering::SeqCst);
        });
        assert!(peak.load(Ordering::SeqCst) <= 1000);
        assert!(peak.load(Ordering::SeqCst) > 450, "never ran two at once");
        assert_eq!(budget.in_use(), 0);
    }

    #[test]
    fn keeps_input_order() {
        let items: Vec<u32> = (0..50).collect();
        assert_eq!(
            par_map(&items, |n| n * 2),
            (0..50).map(|n| n * 2).collect::<Vec<_>>()
        );
    }
}
