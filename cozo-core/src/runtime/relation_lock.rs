/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

//! FIFO admission for relation mutations (shared) and index maintenance (exclusive).
//! Acquire multiple relations in canonical name order, before storage/TT locks.
//! A queued exclusive request stops new shared arrivals, while existing holders
//! drain. Waiting requests keep their queue position across cancellation polls.
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Condvar, Mutex};
use std::thread::{self, ThreadId};
use std::time::Duration;

use miette::{ensure, Result};

use crate::runtime::db::Poison;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Shared,
    Exclusive,
}

#[derive(Default)]
struct State {
    readers: HashMap<ThreadId, usize>,
    writer: Option<ThreadId>,
    queue: VecDeque<u64>,
    next_ticket: u64,
}

#[derive(Default)]
pub(crate) struct RelationLock {
    state: Mutex<State>,
    changed: Condvar,
}

pub(crate) struct RelationGuard<'a> {
    lock: &'a RelationLock,
    owner: ThreadId,
    mode: Mode,
    // Recursive shared admission is thread-specific; guards must not migrate.
    _not_send: PhantomData<Rc<()>>,
}

struct Waiting<'a> {
    lock: &'a RelationLock,
    ticket: u64,
    queued: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.queued {
            let mut state = self.lock.state.lock().unwrap();
            state.queue.retain(|ticket| *ticket != self.ticket);
            self.lock.changed.notify_all();
        }
    }
}

impl RelationLock {
    #[cfg(all(test, feature = "storage-rocksdb"))]
    pub(crate) fn active_readers(&self) -> usize {
        self.state.lock().unwrap().readers.len()
    }

    #[cfg(test)]
    pub(crate) fn queued_len(&self) -> usize {
        self.state.lock().unwrap().queue.len()
    }

    pub(crate) fn read(&self) -> Result<RelationGuard<'_>> {
        self.read_with_poison(&Poison::default())
    }

    #[cfg(test)]
    pub(crate) fn write(&self) -> Result<RelationGuard<'_>> {
        self.write_with_poison(&Poison::default())
    }

    pub(crate) fn read_with_poison(&self, poison: &Poison) -> Result<RelationGuard<'_>> {
        self.acquire(Mode::Shared, poison)
    }

    pub(crate) fn write_with_poison(&self, poison: &Poison) -> Result<RelationGuard<'_>> {
        self.acquire(Mode::Exclusive, poison)
    }

    fn acquire(&self, mode: Mode, poison: &Poison) -> Result<RelationGuard<'_>> {
        poison.check()?;
        let owner = thread::current().id();
        let guard = || RelationGuard {
            lock: self,
            owner,
            mode,
            _not_send: PhantomData,
        };
        let mut state = self.state.lock().unwrap();
        poison.check()?;
        // A recursive shared acquisition must not queue behind a writer that
        // is itself waiting for this thread's existing shared guard to drain.
        if mode == Mode::Shared {
            if let Some(count) = state.readers.get_mut(&owner) {
                *count += 1;
                return Ok(guard());
            }
        }
        ensure!(
            state.writer != Some(owner),
            "recursive exclusive relation admission"
        );
        ensure!(
            mode != Mode::Exclusive || !state.readers.contains_key(&owner),
            "cannot upgrade shared relation admission to exclusive admission"
        );
        if state.queue.is_empty()
            && state.writer.is_none()
            && (mode == Mode::Shared || state.readers.is_empty())
        {
            match mode {
                Mode::Shared => {
                    state.readers.insert(owner, 1);
                }
                Mode::Exclusive => state.writer = Some(owner),
            }
            return Ok(guard());
        }
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.queue.push_back(ticket);
        drop(state);
        // Declared before each state guard, so cancellation/unwind drops the
        // mutex guard before removing the queued request.
        let mut waiting = Waiting {
            lock: self,
            ticket,
            queued: true,
        };
        loop {
            poison.check()?;
            let mut state = self.state.lock().unwrap();
            if state.queue.front() == Some(&ticket)
                && state.writer.is_none()
                && (mode == Mode::Shared || state.readers.is_empty())
            {
                // Cancellation can race a wake-up; check again before admitting.
                poison.check()?;
                state.queue.pop_front();
                waiting.queued = false;
                match mode {
                    Mode::Shared => {
                        state.readers.insert(owner, 1);
                    }
                    Mode::Exclusive => state.writer = Some(owner),
                }
                self.changed.notify_all();
                return Ok(guard());
            }
            let wait = poison
                .deadline
                .map_or(Duration::from_millis(2), |deadline| {
                    deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .min(Duration::from_millis(2))
                });
            drop(self.changed.wait_timeout(state, wait).unwrap());
        }
    }
}

impl Drop for RelationGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.lock.state.lock().unwrap();
        match self.mode {
            Mode::Shared => {
                let count = state.readers.get_mut(&self.owner).unwrap();
                *count -= 1;
                if *count == 0 {
                    state.readers.remove(&self.owner);
                }
            }
            Mode::Exclusive => state.writer = None,
        }
        self.lock.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Instant;

    fn queued(lock: &RelationLock, count: usize) {
        let end = Instant::now() + Duration::from_secs(3);
        while lock.state.lock().unwrap().queue.len() != count {
            assert!(Instant::now() < end, "request reached admission queue");
            thread::yield_now();
        }
    }

    #[test]
    fn queued_writer_precedes_new_reader_and_recursive_reader_can_finish() {
        let lock = RelationLock::default();
        thread::scope(|scope| {
            let lock = &lock;
            let first = lock.read().unwrap();
            let (send, recv) = mpsc::channel();
            let (release, released) = mpsc::channel();
            let writer_send = send.clone();
            scope.spawn(move || {
                let _guard = lock.write().unwrap();
                writer_send.send("writer").unwrap();
                released.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            queued(lock, 1);
            scope.spawn(move || {
                let _guard = lock.read().unwrap();
                send.send("reader").unwrap();
            });
            queued(lock, 2);
            drop(lock.read().unwrap()); // no recursive-reader/writer deadlock
            assert!(recv.try_recv().is_err());
            drop(first);
            assert_eq!(recv.recv_timeout(Duration::from_secs(3)).unwrap(), "writer");
            assert!(recv.try_recv().is_err());
            release.send(()).unwrap();
            assert_eq!(recv.recv_timeout(Duration::from_secs(3)).unwrap(), "reader");
        });
    }

    #[test]
    fn cancelled_writer_removes_reservation_and_wakes_compatible_reader() {
        let lock = RelationLock::default();
        let poison = Poison::default();
        thread::scope(|scope| {
            let lock = &lock;
            let _held = lock.read().unwrap();
            let writer = scope.spawn(|| lock.write_with_poison(&poison).map(drop));
            queued(lock, 1);
            let reader = scope.spawn(|| lock.read().map(drop));
            queued(lock, 2);
            poison.kill();
            assert!(writer.join().unwrap().is_err());
            reader.join().unwrap().unwrap();
            assert!(lock.state.lock().unwrap().queue.is_empty());
        });
        drop(lock.write().unwrap());
    }

    #[test]
    fn timed_out_reader_does_not_strand_queue_and_panic_releases_writer() {
        let lock = RelationLock::default();
        thread::scope(|scope| {
            let lock = &lock;
            let _writer = lock.write().unwrap();
            scope
                .spawn(|| {
                    let poison = Poison {
                        deadline: Some(Instant::now() + Duration::from_millis(10)),
                        ..Default::default()
                    };
                    assert!(lock.read_with_poison(&poison).is_err());
                })
                .join()
                .unwrap();
            assert!(lock.state.lock().unwrap().queue.is_empty());
        });
        let _ = std::panic::catch_unwind(|| {
            let _writer = lock.write().unwrap();
            panic!("exercise guard cleanup");
        });
        drop(lock.read().unwrap());
        let _reader = lock.read().unwrap();
        assert!(
            lock.write().is_err(),
            "upgrades fail instead of self-deadlocking"
        );
    }

    #[test]
    fn queued_reader_precedes_later_writer() {
        let lock = RelationLock::default();
        thread::scope(|scope| {
            let lock = &lock;
            let held = lock.write().unwrap();
            let (send, recv) = mpsc::channel();
            let (release, released) = mpsc::channel();
            let reader_send = send.clone();
            scope.spawn(move || {
                let _reader = lock.read().unwrap();
                reader_send.send("reader").unwrap();
                released.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            queued(lock, 1);
            scope.spawn(move || {
                let _writer = lock.write().unwrap();
                send.send("writer").unwrap();
            });
            queued(lock, 2);
            drop(held);
            assert_eq!(recv.recv_timeout(Duration::from_secs(3)).unwrap(), "reader");
            release.send(()).unwrap();
            assert_eq!(recv.recv_timeout(Duration::from_secs(3)).unwrap(), "writer");
        });
    }

    #[test]
    fn relation_names_are_sorted_and_deduplicated_before_admission() {
        let db = crate::new_cozo_mem().unwrap();
        let a = smartstring::SmartString::from("a");
        let z = smartstring::SmartString::from("z");
        let old_z = db.obtain_relation_locks([&z].into_iter()).pop().unwrap();
        let locks = db.obtain_relation_locks([&z, &a, &a].into_iter());
        assert_eq!(locks.len(), 2);
        assert!(Arc::ptr_eq(&old_z, &locks[1]));
        let again = db.obtain_relation_locks([&a, &z].into_iter());
        assert!(Arc::ptr_eq(&locks[0], &again[0]));
        assert!(Arc::ptr_eq(&locks[1], &again[1]));
    }
}
