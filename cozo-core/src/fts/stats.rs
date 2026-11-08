// SPDX-License-Identifier: MPL-2.0

//! Snapshot-compatible, transaction-local FTS corpus statistics.
//!
//! Shared entries describe committed postings only. A transaction may exchange
//! them only in the generation captured before opening its storage snapshot.
//! Writers keep a fence through commit or storage teardown; overlapping and old
//! snapshots use local scans without publishing their results.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::runtime::relation::RelationId;

pub(crate) type DocStats = (u64, u64);

#[derive(Default)]
struct Committed {
    generation: u64,
    active: usize,
    disabled: bool,
    entries: BTreeMap<RelationId, DocStats>,
}

impl Committed {
    fn advance(&mut self) {
        if let Some(next) = self.generation.checked_add(1) {
            self.generation = next;
        } else {
            self.disabled = true;
            self.entries.clear();
        }
    }
}

#[derive(Default)]
pub(crate) struct FtsStatsCache(Mutex<Committed>);

impl FtsStatsCache {
    fn lock(&self) -> MutexGuard<'_, Committed> {
        self.0.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            // An interrupted bookkeeping operation may have an incomplete
            // generation or writer count. Permanently fall back to local scans.
            state.disabled = true;
            state.entries.clear();
            state
        })
    }

    /// Must run before the storage transaction pins its snapshot.
    pub(crate) fn transaction(self: &Arc<Self>) -> TxFtsStats {
        let generation = self.lock().generation;
        TxFtsStats {
            shared: self.clone(),
            generation,
            local: Mutex::new(Local::default()),
            mutation_started: AtomicBool::new(false),
        }
    }

    /// Raw restore bypasses SessionTx and may replace every index identity.
    pub(crate) fn raw_write(self: &Arc<Self>) -> TxFtsStats {
        let tx = self.transaction();
        tx.start_write(None);
        tx
    }
}

#[derive(Default)]
struct Local {
    entries: BTreeMap<RelationId, DocStats>,
    dirty: BTreeSet<RelationId>,
    uncertain: BTreeSet<RelationId>,
    all: bool,
    active: bool,
    finished: bool,
}

pub(crate) struct TxFtsStats {
    shared: Arc<FtsStatsCache>,
    generation: u64,
    local: Mutex<Local>,
    // Set before any fallible mutation bookkeeping. Read-only transactions still
    // capture the generation before their snapshot, but need no writer cleanup.
    mutation_started: AtomicBool,
}

impl TxFtsStats {
    fn local(&self) -> MutexGuard<'_, Local> {
        self.local.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.entries.clear();
            // Fail closed even if the interrupted operation did not finish
            // registering its dirty index or active-writer fence.
            let mut shared = self.shared.lock();
            shared.disabled = true;
            shared.entries.clear();
            state
        })
    }

    pub(crate) fn get(&self, id: RelationId) -> Option<DocStats> {
        let mut local = self.local();
        if let Some(stats) = local.entries.get(&id) {
            return Some(*stats);
        }
        let shared = self.shared.lock();
        if !shared.disabled
            && shared.generation == self.generation
            && shared.active == usize::from(local.active)
            && !local.all
            && !local.dirty.contains(&id)
        {
            if let Some(stats) = shared.entries.get(&id).copied() {
                local.entries.insert(id, stats);
                return Some(stats);
            }
        }
        None
    }

    /// Install an exact result computed through this transaction's StoreTx.
    /// No cache lock is held while that computation performs storage I/O.
    pub(crate) fn remember(&self, id: RelationId, stats: DocStats) {
        let mut local = self.local();
        local.entries.insert(id, stats);
        let mut shared = self.shared.lock();
        if !shared.disabled
            && shared.generation == self.generation
            && shared.active == usize::from(local.active)
            && !local.all
            && !local.dirty.contains(&id)
        {
            shared.entries.insert(id, stats);
        }
    }

    fn start_write(&self, id: Option<RelationId>) -> bool {
        self.mutation_started.store(true, Ordering::Release);
        let mut local = self.local();
        let mut shared = self.shared.lock();
        if !local.active {
            match shared.active.checked_add(1) {
                Some(active) => shared.active = active,
                None => shared.disabled = true,
            }
            local.active = true;
        }
        if let Some(id) = id {
            let uncertain = !local.uncertain.insert(id);
            local.dirty.insert(id);
            local.entries.remove(&id);
            uncertain
        } else {
            local.all = true;
            local.entries.clear();
            shared.entries.clear();
            true
        }
    }

    /// Invalidate before the first fallible posting write. On partial failure,
    /// a subsequent legacy-transaction query scans its actual pending postings.
    pub(crate) fn before_write(&self, id: RelationId) -> bool {
        self.start_write(Some(id))
    }

    /// A successful coherent operation may resume deltas. After a partial
    /// posting failure, keep scan-only mode until a complete rebuild or tx end.
    pub(crate) fn complete_write(&self, id: RelationId, stats: DocStats, scan_only: bool) {
        self.remember(id, stats);
        if !scan_only {
            self.local().uncertain.remove(&id);
        }
    }

    pub(crate) fn committed(&self) {
        self.finish(true);
    }

    pub(crate) fn can_commit(&self) -> bool {
        !self.mutation_started.load(Ordering::Acquire) || self.local().uncertain.is_empty()
    }

    pub(crate) fn removed(&self, id: RelationId) {
        let mut local = self.local();
        local.entries.remove(&id);
        local.uncertain.remove(&id);
    }

    fn finish(&self, committed: bool) {
        if !self.mutation_started.load(Ordering::Acquire) {
            return;
        }
        let mut local = self.local();
        if local.finished {
            return;
        }
        local.finished = true;
        if !local.active {
            return;
        }
        let mut shared = self.shared.lock();
        let promote = committed
            && !shared.disabled
            && !local.all
            && shared.generation == self.generation
            && shared.active == 1;
        if local.all {
            shared.entries.clear();
        } else {
            for id in &local.dirty {
                shared.entries.remove(id);
            }
        }
        shared.advance();
        if promote && !shared.disabled {
            for id in &local.dirty {
                if let Some(stats) = local.entries.get(id) {
                    shared.entries.insert(*id, *stats);
                }
            }
        }
        if let Some(active) = shared.active.checked_sub(1) {
            shared.active = active;
        } else {
            shared.disabled = true;
            shared.entries.clear();
        }
        local.active = false;
    }
}

// SessionTx declares this field after store_tx. On abort/panic, storage is
// dropped before this guard releases the fence. A failed commit must not call
// committed(): its outcome may be uncertain and its storage may still be live.
impl Drop for TxFtsStats {
    fn drop(&mut self) {
        self.finish(false);
    }
}

#[cfg(test)]
mod tests {
    use super::FtsStatsCache;
    use crate::runtime::relation::RelationId;
    use std::sync::Arc;

    #[test]
    fn unused_reader_still_captures_generation_before_snapshot() {
        let (cache, id) = warmed();
        let reader = cache.transaction();
        let writer = cache.transaction();
        writer.before_write(id);
        writer.complete_write(id, (200, 4), false);
        writer.committed();
        // A fast no-op lifecycle must not delay generation capture until get.
        assert_eq!(reader.get(id), None);
        assert!(reader.can_commit());
        reader.committed();
        assert_eq!(cache.transaction().get(id), Some((200, 4)));
        let failed = cache.transaction();
        failed.before_write(id);
        assert!(!failed.can_commit());
    }

    fn warmed() -> (Arc<FtsStatsCache>, RelationId) {
        let cache = Arc::new(FtsStatsCache::default());
        let id = RelationId(1);
        cache.transaction().remember(id, (100, 3));
        (cache, id)
    }

    #[test]
    fn aborted_writes_never_escape_and_old_readers_cannot_reseed() {
        let (cache, id) = warmed();
        let old = cache.transaction();
        let writer = cache.transaction();
        assert_eq!(writer.get(id), Some((100, 3)));
        writer.before_write(id);
        writer.complete_write(id, (50, 2), false);
        let overlap = cache.transaction();
        assert_eq!(overlap.get(id), None);
        drop(writer);
        assert_eq!(cache.transaction().get(id), None);
        old.remember(id, (100, 3));
        assert_eq!(cache.transaction().get(id), None);
        let fresh = cache.transaction();
        fresh.remember(id, (100, 3));
        assert_eq!(cache.transaction().get(id), Some((100, 3)));
    }

    #[test]
    fn serial_commit_promotes_exact_local_results_once() {
        let (cache, id) = warmed();
        let writer = cache.transaction();
        writer.before_write(id);
        writer.complete_write(id, (120, 4), false);
        writer.committed();
        assert_eq!(cache.transaction().get(id), Some((120, 4)));
        drop(writer);
        assert_eq!(cache.transaction().get(id), Some((120, 4)));
        assert_eq!(cache.lock().active, 0);
    }

    #[test]
    fn overlapping_writers_do_not_publish_incomplete_absolute_totals() {
        let (cache, id) = warmed();
        let a = cache.transaction();
        let b = cache.transaction();
        a.before_write(id);
        b.before_write(id);
        a.complete_write(id, (110, 4), false);
        b.complete_write(id, (120, 4), false);
        a.committed();
        b.committed();
        assert_eq!(cache.transaction().get(id), None);
        assert_eq!(cache.lock().active, 0);
    }

    #[test]
    fn uncertain_operations_require_abort_or_a_complete_rebuild() {
        let (cache, id) = warmed();
        let writer = cache.transaction();
        assert!(!writer.before_write(id));
        // A posting operation failed without installing a complete result.
        assert!(!writer.can_commit());
        writer.remember(id, (90, 3));
        assert!(writer.before_write(id));
        writer.complete_write(id, (80, 2), true);
        assert!(!writer.can_commit());
        // Only an explicit complete rebuild restores the coherent invariant.
        writer.complete_write(id, (100, 3), false);
        assert!(writer.can_commit());
    }

    #[test]
    fn raw_restore_clears_all_entries_and_fences_old_snapshots() {
        let (cache, id) = warmed();
        let old = cache.transaction();
        let restore = cache.raw_write();
        assert_eq!(cache.transaction().get(id), None);
        old.remember(id, (100, 3));
        drop(restore);
        assert_eq!(cache.transaction().get(id), None);
        old.remember(id, (100, 3));
        assert_eq!(cache.transaction().get(id), None);
    }

    #[test]
    fn poisoned_shared_bookkeeping_disables_cache_reuse() {
        let (cache, id) = warmed();
        let poisoned = cache.clone();
        assert!(std::thread::spawn(move || {
            let _guard = poisoned.0.lock().unwrap();
            panic!("injected bookkeeping panic");
        })
        .join()
        .is_err());
        let tx = cache.transaction();
        assert_eq!(tx.get(id), None);
        tx.remember(id, (100, 3));
        assert_eq!(tx.get(id), Some((100, 3)));
        assert_eq!(cache.transaction().get(id), None);
    }
}
