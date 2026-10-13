//! The review slots and the order reviews wait for them in (#401): a
//! limit, the slots in use, and the runs waiting, first to start first. A
//! waiting run can be moved to any place; a superseding commit takes the
//! place of the review it replaces.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use henk_domain::run::RunId;
use tokio::sync::{Notify, watch};

/// The slots and the queue for them.
pub(crate) struct SlotQueue {
    state: Mutex<State>,
    /// A slot was given back, or the queue moved: the first in line looks.
    freed: Notify,
    /// Marked whenever the slots or the queue read differently.
    changed: Arc<watch::Sender<()>>,
}

struct State {
    limit: usize,
    in_use: usize,
    waiting: VecDeque<RunId>,
}

/// A run that is not waiting for a slot here: it started, ended, or
/// another process has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotWaiting;

/// A slot held: given back when dropped.
pub(crate) struct Slot(Arc<SlotQueue>);

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.in_use = state.in_use.saturating_sub(1);
        }
        self.0.freed.notify_waiters();
        self.0.changed.send_replace(());
    }
}

/// A run in the queue while it waits: it leaves the queue when the wait
/// ends without a slot, cancelled or dropped.
struct InLine<'a> {
    queue: &'a SlotQueue,
    run: &'a RunId,
    served: bool,
}

impl Drop for InLine<'_> {
    fn drop(&mut self) {
        if !self.served {
            self.queue.leave(self.run);
        }
    }
}

impl SlotQueue {
    /// `limit` slots, at least one, none in use and nobody waiting.
    pub(crate) fn new(limit: usize, changed: Arc<watch::Sender<()>>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                limit: limit.max(1),
                in_use: 0,
                waiting: VecDeque::new(),
            }),
            freed: Notify::new(),
            changed,
        })
    }

    /// Puts `run` at the back of the queue, unless it is in it already
    /// (a superseding commit put it in its predecessor's place).
    pub(crate) fn join(&self, run: &RunId) {
        if let Ok(mut state) = self.state.lock()
            && !state.waiting.contains(run)
        {
            state.waiting.push_back(run.clone());
        }
        self.changed.send_replace(());
    }

    /// A slot for `run` now, when it is first in line and one is free.
    pub(crate) fn try_take(self: &Arc<Self>, run: &RunId) -> Option<Slot> {
        let taken = {
            let mut state = self.state.lock().ok()?;
            let first = state.waiting.front().is_none_or(|front| front == run);
            if first && state.in_use < state.limit {
                if state.waiting.front() == Some(run) {
                    state.waiting.pop_front();
                }
                state.in_use += 1;
                true
            } else {
                false
            }
        };
        if !taken {
            return None;
        }
        // The next in line may fit too.
        self.freed.notify_waiters();
        self.changed.send_replace(());
        Some(Slot(Arc::clone(self)))
    }

    /// Waits until `run`, which [`Self::join`] put in line, is first in
    /// line and a slot is free, and takes it. Dropped before then, `run`
    /// leaves the queue.
    pub(crate) async fn wait(self: &Arc<Self>, run: &RunId) -> Slot {
        let mut in_line = InLine {
            queue: self,
            run,
            served: false,
        };
        loop {
            // Listening before looking, so a slot freed in between is not
            // missed.
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if let Some(slot) = self.try_take(run) {
                in_line.served = true;
                return slot;
            }
            freed.await;
        }
    }

    /// Takes `run` out of the queue, if it is in it.
    pub(crate) fn leave(&self, run: &RunId) {
        let left = self.state.lock().is_ok_and(|mut state| {
            let before = state.waiting.len();
            state.waiting.retain(|waiting| waiting != run);
            state.waiting.len() != before
        });
        if left {
            self.freed.notify_waiters();
            self.changed.send_replace(());
        }
    }

    /// Puts `new` in the place `old` waits in; false when `old` is not
    /// waiting.
    pub(crate) fn replace(&self, old: &RunId, new: &RunId) -> bool {
        let replaced = self.state.lock().is_ok_and(|mut state| {
            match state.waiting.iter_mut().find(|waiting| *waiting == old) {
                Some(place) => {
                    *place = new.clone();
                    true
                }
                None => false,
            }
        });
        if replaced {
            self.changed.send_replace(());
        }
        replaced
    }

    /// Moves `run` to `position` in the queue, 1 being the next to start;
    /// a position past the end is the last. Returns where it was and where
    /// it is now.
    ///
    /// # Errors
    ///
    /// [`NotWaiting`] when `run` is not in the queue.
    pub(crate) fn move_to(
        &self,
        run: &RunId,
        position: usize,
    ) -> Result<(usize, usize), NotWaiting> {
        let moved = {
            let mut state = self.state.lock().map_err(|_| NotWaiting)?;
            let from = state
                .waiting
                .iter()
                .position(|waiting| waiting == run)
                .ok_or(NotWaiting)?;
            let entry = state.waiting.remove(from).ok_or(NotWaiting)?;
            let to = position.max(1).min(state.waiting.len() + 1) - 1;
            state.waiting.insert(to, entry);
            (from + 1, to + 1)
        };
        // A new first in line may fit where the old one did not.
        self.freed.notify_waiters();
        self.changed.send_replace(());
        Ok(moved)
    }

    /// The limit and the slots in use.
    pub(crate) fn usage(&self) -> (usize, usize) {
        self.state
            .lock()
            .map_or((0, 0), |state| (state.limit, state.in_use))
    }

    /// The runs waiting, first to start first.
    pub(crate) fn order(&self) -> Vec<RunId> {
        self.state.lock().map_or_else(
            |_| Vec::new(),
            |state| state.waiting.iter().cloned().collect(),
        )
    }

    /// Takes a slot past the queue, so a test can fill them.
    #[cfg(test)]
    pub(crate) fn take_any(self: &Arc<Self>) -> Option<Slot> {
        {
            let mut state = self.state.lock().ok()?;
            if state.in_use >= state.limit {
                return None;
            }
            state.in_use += 1;
        }
        self.changed.send_replace(());
        Some(Slot(Arc::clone(self)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use super::*;

    fn run(n: u32) -> RunId {
        RunId::parse(format!("r-20261013-0000000{n}")).unwrap()
    }

    fn queue(limit: usize) -> Arc<SlotQueue> {
        SlotQueue::new(limit, Arc::new(watch::Sender::new(())))
    }

    #[tokio::test]
    async fn runs_take_slots_in_the_order_they_wait_in_and_a_move_changes_it() {
        let q = queue(1);
        let held = q.take_any().unwrap();
        for n in 1..=3 {
            q.join(&run(n));
        }
        assert_eq!(q.order(), [run(1), run(2), run(3)]);
        assert_eq!(q.move_to(&run(3), 1), Ok((3, 1)));
        assert_eq!(q.order(), [run(3), run(1), run(2)]);
        assert!(q.try_take(&run(1)).is_none(), "not first in line");
        let first = {
            let q = Arc::clone(&q);
            tokio::spawn(async move { q.wait(&run(3)).await })
        };
        let second = {
            let q = Arc::clone(&q);
            tokio::spawn(async move { q.wait(&run(1)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!first.is_finished() && !second.is_finished());
        drop(held);
        let slot = first.await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!second.is_finished(), "one slot");
        drop(slot);
        drop(second.await.unwrap());
        assert_eq!(q.order(), [run(2)]);
        assert_eq!(q.usage(), (1, 0));
    }

    #[tokio::test]
    async fn a_move_past_the_end_is_last_and_a_run_not_waiting_cannot_move() {
        let q = queue(1);
        let _held = q.take_any().unwrap();
        q.join(&run(1));
        q.join(&run(2));
        assert_eq!(q.move_to(&run(1), 9), Ok((1, 2)));
        assert_eq!(q.move_to(&run(1), 0), Ok((2, 1)), "0 is read as the first");
        assert_eq!(q.move_to(&run(7), 1), Err(NotWaiting));
    }

    #[tokio::test]
    async fn a_wait_dropped_or_a_replaced_run_leaves_no_one_behind() {
        let q = queue(1);
        let held = q.take_any().unwrap();
        q.join(&run(1));
        let waiting = {
            let q = Arc::clone(&q);
            tokio::spawn(async move { q.wait(&run(1)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(q.order(), [run(1)]);
        waiting.abort();
        let _ = waiting.await;
        assert!(q.order().is_empty(), "a dropped wait leaves the queue");
        q.join(&run(2));
        q.join(&run(3));
        assert!(q.replace(&run(2), &run(4)));
        assert_eq!(q.order(), [run(4), run(3)], "in the place it replaced");
        q.leave(&run(2));
        assert_eq!(q.order(), [run(4), run(3)], "the old one is gone already");
        drop(held);
        assert!(q.try_take(&run(4)).is_some());
    }
}
