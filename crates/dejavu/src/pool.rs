//! `mapPool` (`concurrency.ts`): bounded parallel map on scoped threads.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Runs `mapper(item, index)` over `items` on at most `max_parallel` scoped
/// threads and returns the results in input order. Workers take the next
/// unclaimed item, so slow items do not hold back the rest. With one worker (or
/// one item) it runs on the calling thread. A panicking mapper propagates.
/// Errors when `max_parallel` is 0, as `mapPool` did.
pub fn map_pool<T, U, F>(items: &[T], max_parallel: usize, mapper: F) -> Result<Vec<U>, String>
where
    T: Sync,
    U: Send,
    F: Fn(&T, usize) -> U + Sync,
{
    if max_parallel < 1 {
        return Err(format!(
            "maxParallel must be an integer >= 1 (got '{max_parallel}')"
        ));
    }
    let workers = max_parallel.min(items.len());
    if workers <= 1 {
        return Ok(items
            .iter()
            .enumerate()
            .map(|(index, item)| mapper(item, index))
            .collect());
    }
    let next = AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<U>>> =
        Mutex::new(std::iter::repeat_with(|| None).take(items.len()).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else { return };
                    let result = mapper(item, index);
                    slots.lock().unwrap_or_else(|poison| poison.into_inner())[index] = Some(result);
                }
            });
        }
    });
    let slots = slots
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner());
    Ok(slots
        .into_iter()
        .map(|slot| slot.expect("every index is mapped once"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn keeps_input_order_and_bounds_concurrency() {
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let items: Vec<u64> = (0..12).collect();
        let results = map_pool(&items, 3, |item, index| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            // Later items finish first, so order must come from the index.
            std::thread::sleep(Duration::from_millis(12 - item));
            active.fetch_sub(1, Ordering::SeqCst);
            (index as u64, item * 2)
        })
        .unwrap();
        assert_eq!(
            results,
            items
                .iter()
                .map(|&item| (item, item * 2))
                .collect::<Vec<_>>()
        );
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert!(peak.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn handles_small_inputs_and_rejects_zero_workers() {
        assert_eq!(
            map_pool(&[] as &[u8], 4, |item, _| *item).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(map_pool(&[7], 4, |item, index| item + index).unwrap(), [7]);
        assert_eq!(map_pool(&[1, 2], 1, |item, _| item * 10).unwrap(), [10, 20]);
        assert_eq!(
            map_pool(&[1], 0, |item, _| *item).unwrap_err(),
            "maxParallel must be an integer >= 1 (got '0')"
        );
    }
}
