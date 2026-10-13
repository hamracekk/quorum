//! A small order-preserving parallel map over scoped threads (no extra dependency): research
//! runs are independent backtests, so they spread over the cores while the results stay in
//! input order — and therefore bit-for-bit the same as a serial run.

use std::num::NonZeroUsize;
use std::thread;

/// `f` applied to every item, using up to all available cores; results in input order.
pub fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .min(items.len());
    if threads <= 1 {
        return items.iter().map(&f).collect();
    }
    let chunk = items.len().div_ceil(threads);
    thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| {
                let f = &f;
                scope.spawn(move || part.iter().map(f).collect::<Vec<R>>())
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| {
                h.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_keep_input_order() {
        let items: Vec<u64> = (0..1_000).collect();
        let squares = par_map(&items, |x| x * x);
        assert_eq!(squares, items.iter().map(|x| x * x).collect::<Vec<_>>());
        assert!(par_map(&Vec::<u64>::new(), |x| *x).is_empty());
    }
}
