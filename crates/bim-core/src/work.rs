//! Running independent work on several threads without changing the order of
//! the result.
//!
//! Every stage of this pipeline that costs real time is a loop over items that
//! do not depend on each other - a partition's members, a product's geometry,
//! an element's triangles, a block of properties - inside a pass whose output
//! has to stay in the order the items came in. Each of those loops was about
//! to grow its own scoped-thread block, which is the same reason
//! [`crate`](crate) already holds the model they all pass around: one answer,
//! in the one crate every reader and writer depends on.
//!
//! What this guarantees is what makes it safe to use in a decoder: the results
//! are handed back in the order of the input, whatever the machine, so a
//! conversion run on one core and on thirty-two produces the same bytes. The
//! work itself must therefore not depend on the pass so far - that is the
//! caller's part of the bargain, and it is why a fold that does depend on it
//! stays on the calling thread.

/// Threads to spread `work` items over: what the machine reports, and never
/// more than there is work for.
#[must_use]
pub fn threads(work: usize) -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(work.max(1))
}

/// Apply `map` to every item on as many threads as the machine has, and hand
/// the results back in the order the items came in.
///
/// A panic inside `map` is carried out to the calling thread rather than
/// swallowed, so a failure reads the way it would have read in a plain loop.
pub fn map_in_order<I: Sync, T: Send>(items: &[I], map: impl Fn(&I) -> T + Sync) -> Vec<T> {
    let per_thread = items.len().div_ceil(threads(items.len()));
    if per_thread == 0 {
        return Vec::new();
    }
    let map = &map;
    std::thread::scope(|scope| {
        let workers = items
            .chunks(per_thread)
            .map(|chunk| scope.spawn(move || chunk.iter().map(map).collect::<Vec<T>>()))
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_order_of_the_input() {
        let items = (0..1000_u32).collect::<Vec<_>>();
        let doubled = map_in_order(&items, |value| value * 2);
        assert_eq!(
            doubled,
            items.iter().map(|value| value * 2).collect::<Vec<_>>()
        );
    }

    #[test]
    fn maps_an_empty_input_to_an_empty_result() {
        let empty: Vec<u32> = Vec::new();
        assert!(map_in_order(&empty, |value| *value).is_empty());
    }

    #[test]
    fn carries_a_panic_out_to_the_caller() {
        let items = (0..64_u32).collect::<Vec<_>>();
        let result = std::panic::catch_unwind(|| {
            map_in_order(&items, |value| {
                assert!(*value != 40, "the mapped work failed");
                *value
            })
        });
        assert!(result.is_err());
    }
}
