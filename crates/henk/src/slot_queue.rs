//! The slots for runs on pull requests and the order runs wait for them in
//! (#401, #580): a limit, the slots in use, and the runs waiting, first to
//! start first. A higher priority waits ahead of a lower one: a rebase
//! ahead of every review. Within its priority a waiting run can be moved
//! to any place; a superseding run takes the place of the one it replaces.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use henk_domain::queue::Priority;
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
    /// Sorted by priority, highest first; within one, in arrival order.
    waiting: VecDeque<(RunId, Priority)>,
}

impl State {
    fn place_of(&self, run: &RunId) -> Option<usize> {
        self.waiting.iter().position(|(waiting, _)| waiting == run)
    }

    /// Puts `run` behind every run of its priority or higher.
    fn insert(&mut self, run: RunId, priority: Priority) {
        let at = self
            .waiting
            .iter()
            .position(|(_, waiting)| *waiting < priority)
            .unwrap_or(self.waiting.len());
        self.waiting.insert(at, (run, priority));
    }

    /// The places runs of `priority` may take, from the first to one past
    /// the last.
    fn band(&self, priority: Priority) -> (usize, usize) {
        let start = self
            .waiting
            .iter()
            .take_while(|(_, p)| *p > priority)
            .count();
        let len = self.waiting.iter().filter(|(_, p)| *p == priority).count();
        (start, start + len)
    }
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

    /// Puts `run` behind every run waiting at its priority or higher,
    /// unless it is in the queue already (a superseding run put it in its
    /// predecessor's place).
    pub(crate) fn join(&self, run: &RunId, priority: Priority) {
        if let Ok(mut state) = self.state.lock()
            && state.place_of(run).is_none()
        {
            state.insert(run.clone(), priority);
        }
        self.changed.send_replace(());
    }

    /// A slot for `run` now, when it is first in line and one is free.
    pub(crate) fn try_take(self: &Arc<Self>, run: &RunId) -> Option<Slot> {
        let taken = {
            let mut state = self.state.lock().ok()?;
            let front = state.waiting.front().map(|(front, _)| front);
            let first = front.is_none_or(|front| front == run);
            if first && state.in_use < state.limit {
                if front == Some(run) {
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
            state.waiting.retain(|(waiting, _)| waiting != run);
            state.waiting.len() != before
        });
        if left {
            self.freed.notify_waiters();
            self.changed.send_replace(());
        }
    }

    /// Puts `new` in the place `old` waits in, or, at another priority,
    /// where that priority waits; false when `old` is not waiting.
    pub(crate) fn replace(&self, old: &RunId, new: &RunId, priority: Priority) -> bool {
        let replaced = self.state.lock().is_ok_and(|mut state| {
            let Some(at) = state.place_of(old) else {
                return false;
            };
            match state.waiting.get_mut(at) {
                Some(place) if place.1 == priority => place.0 = new.clone(),
                _ => {
                    state.waiting.remove(at);
                    state.insert(new.clone(), priority);
                }
            }
            true
        });
        if replaced {
            self.changed.send_replace(());
        }
        replaced
    }

    /// Moves `run` to `position` in the queue, 1 being the next to start,
    /// but never out of its priority: a review stays behind every waiting
    /// rebase. A position past its priority's places is the nearest one.
    /// Returns where it was and where it is now.
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
            let from = state.place_of(run).ok_or(NotWaiting)?;
            let entry = state.waiting.remove(from).ok_or(NotWaiting)?;
            let (first, end) = state.band(entry.1);
            let to = (position.max(1) - 1).clamp(first, end);
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
            |state| state.waiting.iter().map(|(run, _)| run.clone()).collect(),
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
            q.join(&run(n), Priority::Review);
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
        q.join(&run(1), Priority::Review);
        q.join(&run(2), Priority::Review);
        assert_eq!(q.move_to(&run(1), 9), Ok((1, 2)));
        assert_eq!(q.move_to(&run(1), 0), Ok((2, 1)), "0 is read as the first");
        assert_eq!(q.move_to(&run(7), 1), Err(NotWaiting));
    }

    #[tokio::test]
    async fn a_wait_dropped_or_a_replaced_run_leaves_no_one_behind() {
        let q = queue(1);
        let held = q.take_any().unwrap();
        q.join(&run(1), Priority::Review);
        let waiting = {
            let q = Arc::clone(&q);
            tokio::spawn(async move { q.wait(&run(1)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(q.order(), [run(1)]);
        waiting.abort();
        let _ = waiting.await;
        assert!(q.order().is_empty(), "a dropped wait leaves the queue");
        q.join(&run(2), Priority::Review);
        q.join(&run(3), Priority::Review);
        assert!(q.replace(&run(2), &run(4), Priority::Review));
        assert_eq!(q.order(), [run(4), run(3)], "in the place it replaced");
        q.leave(&run(2));
        assert_eq!(q.order(), [run(4), run(3)], "the old one is gone already");
        drop(held);
        assert!(q.try_take(&run(4)).is_some());
    }

    #[tokio::test]
    async fn a_rebase_waits_ahead_of_every_review_and_a_move_keeps_it_there() {
        let q = queue(1);
        let held = q.take_any().unwrap();
        q.join(&run(1), Priority::Review);
        q.join(&run(2), Priority::Review);
        q.join(&run(3), Priority::Rebase);
        q.join(&run(4), Priority::Rebase);
        assert_eq!(
            q.order(),
            [run(3), run(4), run(1), run(2)],
            "rebases first, in arrival order"
        );
        assert_eq!(q.move_to(&run(2), 1), Ok((4, 3)), "not ahead of a rebase");
        assert_eq!(q.move_to(&run(4), 9), Ok((2, 2)), "not behind a review");
        assert_eq!(q.move_to(&run(4), 1), Ok((2, 1)));
        assert_eq!(q.order(), [run(4), run(3), run(2), run(1)]);
        assert!(q.replace(&run(1), &run(5), Priority::Rebase));
        assert_eq!(
            q.order(),
            [run(4), run(3), run(5), run(2)],
            "a rebase replacing a review moves up"
        );
        drop(held);
        assert!(
            q.try_take(&run(2)).is_none(),
            "a review waits for the rebases"
        );
        assert!(q.try_take(&run(4)).is_some());
    }
}
