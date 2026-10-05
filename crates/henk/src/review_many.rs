//! Several reviews from one command (§3.1: up to 50 at once), at most a
//! configured number running at the same time. Every review is attempted;
//! one failing does not stop the others.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// A review whose task ended without a result: it panicked or was aborted.
/// It counts as a failure like any other.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the review task {0}")]
pub struct TaskFailed(String);

/// Runs `review` for every item, at most `limit` at a time, and returns one
/// result per item, in the order of `items`. A task that panics comes back
/// as [`TaskFailed`] for its item; it is never left out.
pub async fn review_all<T, R, F, Fut>(
    items: Vec<T>,
    limit: usize,
    review: F,
) -> Vec<(T, Result<R, TaskFailed>)>
where
    T: Clone + Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> Fut,
    Fut: Future<Output = R> + Send + 'static,
{
    let slots = Arc::new(Semaphore::new(limit.max(1)));
    let mut set = JoinSet::new();
    let mut index_of = HashMap::new();
    for (index, item) in items.iter().cloned().enumerate() {
        let slots = Arc::clone(&slots);
        let work = review(item);
        let handle = set.spawn(async move {
            // The semaphore is never closed, so acquiring only waits.
            let _permit = slots.acquire_owned().await.ok();
            work.await
        });
        index_of.insert(handle.id(), index);
    }
    let mut results: Vec<Option<Result<R, TaskFailed>>> = items.iter().map(|_| None).collect();
    while let Some(joined) = set.join_next_with_id().await {
        let (id, result) = match joined {
            Ok((id, result)) => (id, Ok(result)),
            Err(error) => {
                let what = if error.is_panic() {
                    "panicked"
                } else {
                    "was cancelled"
                };
                (error.id(), Err(TaskFailed(what.to_owned())))
            }
        };
        if let Some(slot) = index_of.get(&id).and_then(|i| results.get_mut(*i)) {
            *slot = Some(result);
        }
    }
    items
        .into_iter()
        .zip(results)
        .map(|(item, result)| {
            let result =
                result.unwrap_or_else(|| Err(TaskFailed("ended without a result".to_owned())));
            (item, result)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn at_most_limit_run_at_once_and_results_keep_their_order() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let results = review_all(vec![1_u64, 2, 3, 4, 5], 2, |n| {
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                // Later items finish first, so order must come from the index.
                tokio::time::sleep(Duration::from_millis(30 - 5 * n)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                if n == 3 {
                    Err(format!("review {n} failed"))
                } else {
                    Ok(n * 10)
                }
            }
        })
        .await;
        assert_eq!(peak.load(Ordering::SeqCst), 2, "never more than the limit");
        let items: Vec<u64> = results.iter().map(|(n, _)| *n).collect();
        assert_eq!(items, vec![1, 2, 3, 4, 5]);
        assert_eq!(results[2].1, Ok(Err("review 3 failed".to_owned())));
        assert_eq!(
            results[4].1,
            Ok(Ok(50)),
            "a failure does not stop the others"
        );
    }

    #[tokio::test]
    async fn a_zero_limit_still_runs_one_at_a_time() {
        let results = review_all(vec!["a"], 0, |s| async move { s.len() }).await;
        assert_eq!(results, vec![("a", Ok(1))]);
    }

    #[tokio::test]
    async fn a_panicking_review_is_a_failure_in_its_place_not_a_gap() {
        let results = review_all(vec![1_u32, 2, 3], 3, |n| async move {
            assert!(n != 2, "review {n} crashed");
            n
        })
        .await;
        let items: Vec<u32> = results.iter().map(|(n, _)| *n).collect();
        assert_eq!(items, vec![1, 2, 3], "nothing is left out");
        assert_eq!(results[0].1, Ok(1));
        assert_eq!(
            results[1].1,
            Err(TaskFailed("panicked".to_owned())),
            "the panic is reported for its own item"
        );
        assert_eq!(results[2].1, Ok(3));
    }
}
