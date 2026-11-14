// SPDX-License-Identifier: MPL-2.0

//! Persisted BM25 corpus totals (mnestic fork, opt-in, RocksDB only).
//!
//! Without this, a search whose transaction cannot prove the in-memory totals
//! match its snapshot (after open, or overlapping a write) scans every posting
//! of the index. Here each full-text index carries, inside its own key range
//! and above every posting key:
//!
//! - a **base** record `[Bot, "stats"]` = `(epoch, generation, tokens, docs)`;
//! - one **delta** record `[Bot, "stats", <random id>]` = `(epoch, tokens, docs)`
//!   per write transaction that changed the index.
//!
//! A writer writes only its own delta key, so writers never contend. A reader
//! in a read-only transaction sums the base and the deltas in its pinned
//! snapshot: exact for that snapshot, one short range read, no posting scan.
//! A seed computes the base by one exact scan off the request path.
//!
//! Records are trusted only for the current **epoch**. At enable time the
//! engine compares RocksDB's latest sequence number with the marker written at
//! the previous clean close; if anything else wrote to the store in between
//! (an older engine, this engine with persistence off, a crash, a restore),
//! it picks a new random epoch and every existing record becomes invisible
//! until the index is seeded again. Index build and reindex delete the base,
//! which also leaves the index unseeded. An unseeded index behaves exactly as
//! without this feature.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use miette::Result;

use crate::data::tuple::TupleT;
use crate::data::value::{DataValue, LARGEST_UTF_CHAR};
use crate::runtime::relation::{RelationHandle, RelationId};
use crate::runtime::transact::SessionTx;

const BASE_LEN: usize = 32;
const DELTA_LEN: usize = 24;

/// Shared per-`Db` state. Holds the close hook that writes the marker; it runs
/// when the last handle to the database is dropped.
#[derive(Default)]
pub(crate) struct FtsPersist {
    enabled: AtomicBool,
    epoch: AtomicU64,
    closer: Mutex<Option<Box<dyn FnOnce(u64) + Send>>>,
}

impl FtsPersist {
    pub(crate) fn epoch(&self) -> Option<u64> {
        self.enabled
            .load(Ordering::Acquire)
            .then(|| self.epoch.load(Ordering::Acquire))
    }

    pub(crate) fn enable(&self, epoch: u64, closer: Box<dyn FnOnce(u64) + Send>) {
        self.epoch.store(epoch, Ordering::Release);
        *self.closer.lock().unwrap_or_else(|p| p.into_inner()) = Some(closer);
        self.enabled.store(true, Ordering::Release);
    }

    /// Stop maintaining records and do not write a marker at close, so the
    /// next enabled open distrusts whatever is on disk.
    pub(crate) fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
        self.closer.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

impl Drop for FtsPersist {
    fn drop(&mut self) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        let closer = self.closer.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(closer) = closer {
            closer(self.epoch.load(Ordering::Acquire));
        }
    }
}

/// Outcome of [`crate::Db::enable_persisted_fts_stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtsStatsOpen {
    /// This storage backend does not support persisted totals; nothing changed.
    Unsupported,
    /// Nothing wrote to the store since the last clean close; existing
    /// records are trusted.
    Trusted,
    /// Existing records (if any) were invalidated; indexes stay unseeded until
    /// [`crate::Db::seed_fts_stats`] runs.
    Reset,
}

/// One row of [`crate::Db::verify_fts_stats`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsStatsCheck {
    /// The index relation, `relation:index`.
    pub index: String,
    /// `(tokens, documents)` from the records, `None` when unseeded.
    pub persisted: Option<(u64, u64)>,
    /// `(tokens, documents)` from a full posting scan in the same snapshot.
    pub scanned: (u64, u64),
    /// Delta records not yet folded into the base.
    pub deltas: usize,
}

pub(crate) fn marker_key() -> Vec<u8> {
    vec![DataValue::Null, DataValue::from("FTS_STATS_MARKER")].encode_as_key(RelationId::SYSTEM)
}

pub(crate) fn encode_marker(seq: u64, epoch: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    v.extend(seq.to_be_bytes());
    v.extend(epoch.to_be_bytes());
    v
}

pub(crate) fn decode_marker(v: &[u8]) -> Option<(u64, u64)> {
    (v.len() == 16).then(|| (be_u64(&v[0..8]), be_u64(&v[8..16])))
}

fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b.try_into().expect("8 bytes"))
}

fn base_key(idx: &RelationHandle) -> Vec<u8> {
    idx.encode_partial_key_for_store(&[DataValue::Bot, DataValue::from("stats")])
}

fn records_upper(idx: &RelationHandle) -> Vec<u8> {
    idx.encode_partial_key_for_store(&[DataValue::Bot, DataValue::from("stats"), DataValue::Bot])
}

/// True for keys of the statistics records of `idx`; reindex must not delete
/// them (another writer's just-committed delta would make it conflict).
pub(crate) fn is_record_key(idx: &RelationHandle, key: &[u8]) -> bool {
    let lower = base_key(idx);
    key >= lower.as_slice() && key < records_upper(idx).as_slice()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Base {
    epoch: u64,
    generation: u64,
    tokens: u64,
    docs: u64,
}

fn encode_base(b: Base) -> Vec<u8> {
    let mut v = Vec::with_capacity(BASE_LEN);
    for x in [b.epoch, b.generation, b.tokens, b.docs] {
        v.extend(x.to_be_bytes());
    }
    v
}

fn decode_base(v: &[u8]) -> Option<Base> {
    (v.len() == BASE_LEN).then(|| Base {
        epoch: be_u64(&v[0..8]),
        generation: be_u64(&v[8..16]),
        tokens: be_u64(&v[16..24]),
        docs: be_u64(&v[24..32]),
    })
}

fn decode_delta(v: &[u8]) -> Option<(u64, i64, i64)> {
    (v.len() == DELTA_LEN).then(|| {
        (
            be_u64(&v[0..8]),
            be_u64(&v[8..16]) as i64,
            be_u64(&v[16..24]) as i64,
        )
    })
}

/// What a transaction saw of one index's records.
struct Records {
    base_raw: Option<Vec<u8>>,
    base: Option<Base>,
    /// Sum of deltas carrying the current epoch.
    delta_tokens: i128,
    delta_docs: i128,
    /// Every delta key seen, whatever its epoch.
    delta_keys: Vec<Vec<u8>>,
    malformed: bool,
}

impl<'a> SessionTx<'a> {
    fn read_fts_records(&self, idx: &RelationHandle, epoch: u64) -> Result<Records> {
        let lower = base_key(idx);
        let upper = records_upper(idx);
        let mut out = Records {
            base_raw: None,
            base: None,
            delta_tokens: 0,
            delta_docs: 0,
            delta_keys: Vec::new(),
            malformed: false,
        };
        for item in self.store_tx.range_scan(&lower, &upper) {
            let (k, v) = item?;
            if k.as_slice() >= upper.as_slice() {
                break;
            }
            if k == lower {
                out.base = decode_base(&v);
                out.malformed |= out.base.is_none();
                out.base_raw = Some(v);
                continue;
            }
            match decode_delta(&v) {
                Some((e, dt, dd)) => {
                    if e == epoch {
                        out.delta_tokens += i128::from(dt);
                        out.delta_docs += i128::from(dd);
                    }
                }
                None => out.malformed = true,
            }
            out.delta_keys.push(k);
        }
        Ok(out)
    }

    /// Totals from the records, or `None` when persistence is off, the index
    /// is unseeded, or the records fail the sanity guard (logged). Inside a
    /// write transaction this adds the transaction's own pending change.
    pub(crate) fn persisted_fts_doc_stats(
        &self,
        idx: &RelationHandle,
    ) -> Result<Option<(u64, u64)>> {
        let Some(epoch) = self.fts_persist.epoch() else {
            return Ok(None);
        };
        let records = self.read_fts_records(idx, epoch)?;
        let Some(base) = records.base.filter(|b| b.epoch == epoch) else {
            return Ok(None);
        };
        let (pt, pd) = self
            .fts_pending
            .get(&idx.id)
            .map_or((0, 0), |(_, t, d)| (i128::from(*t), i128::from(*d)));
        let tokens = i128::from(base.tokens) + records.delta_tokens + pt;
        let docs = i128::from(base.docs) + records.delta_docs + pd;
        let sane = !records.malformed
            && tokens >= 0
            && docs >= 0
            && (tokens == 0) == (docs == 0)
            && tokens <= i128::from(u64::MAX)
            && docs <= i128::from(u64::MAX);
        if !sane {
            log::warn!(
                "fts.persisted_stats_invalid: index {} has unusable persisted totals \
                 (tokens {tokens}, documents {docs}, malformed {}); falling back to a scan \
                 until it is seeded again",
                idx.name,
                records.malformed
            );
            return Ok(None);
        }
        Ok(Some((tokens as u64, docs as u64)))
    }

    /// Record a posting change for the delta written at commit.
    pub(crate) fn record_fts_delta(&mut self, idx: &RelationHandle, tokens: i64, docs: i64) {
        if self.fts_persist.epoch().is_none() || (tokens == 0 && docs == 0) {
            return;
        }
        let entry = self
            .fts_pending
            .entry(idx.id)
            .or_insert_with(|| (idx.clone(), 0, 0));
        entry.1 += tokens;
        entry.2 += docs;
    }

    /// Write this transaction's delta records. Called immediately before the
    /// storage commit, after every posting write (including transaction-time
    /// stamping).
    pub(crate) fn write_fts_deltas(&mut self) -> Result<()> {
        let Some(epoch) = self.fts_persist.epoch() else {
            self.fts_pending.clear();
            return Ok(());
        };
        let pending = std::mem::take(&mut self.fts_pending);
        for (_, (idx, tokens, docs)) in pending {
            if tokens == 0 && docs == 0 {
                continue;
            }
            // A random 128-bit id: no two transactions write the same key,
            // including across restarts and restores.
            let key = idx.encode_partial_key_for_store(&[
                DataValue::Bot,
                DataValue::from("stats"),
                DataValue::from(uuid::Uuid::new_v4().simple().to_string()),
            ]);
            let mut val = Vec::with_capacity(DELTA_LEN);
            val.extend(epoch.to_be_bytes());
            val.extend((tokens as u64).to_be_bytes());
            val.extend((docs as u64).to_be_bytes());
            self.store_tx.put(&key, &val)?;
        }
        Ok(())
    }

    /// Index build and reindex: delete the base so the index is unseeded.
    /// Deltas stay; the next seed deletes the ones its scan includes.
    /// Deleting only the base avoids conflicting with concurrent writers,
    /// whose delta keys this transaction never writes.
    pub(crate) fn drop_fts_base(&mut self, idx: &RelationHandle) -> Result<()> {
        self.fts_pending.remove(&idx.id);
        let key = base_key(idx);
        if self.store_tx.exists(&key, false)? {
            self.store_tx.del(&key)?;
        }
        Ok(())
    }
}

/// Every full-text index in the catalog.
pub(crate) fn list_fts_indexes(tx: &SessionTx<'_>) -> Result<Vec<RelationHandle>> {
    let lower = vec![DataValue::from("")].encode_as_key(RelationId::SYSTEM);
    let upper =
        vec![DataValue::from(String::from(LARGEST_UTF_CHAR))].encode_as_key(RelationId::SYSTEM);
    let mut out = Vec::new();
    for item in tx.store_tx.range_scan(&lower, &upper) {
        let (k, v) = item?;
        if upper <= k {
            break;
        }
        let handle = RelationHandle::decode(&v)?;
        for (idx, _) in handle.fts_indices.values() {
            out.push(idx.clone());
        }
    }
    Ok(out)
}

/// Read-only half of a seed: exact scan and the records, in one snapshot.
pub(crate) struct SeedPlan {
    base_raw: Option<Vec<u8>>,
    generation: u64,
    tokens: u64,
    docs: u64,
    delta_keys: Vec<Vec<u8>>,
}

pub(crate) fn plan_seed(tx: &SessionTx<'_>, idx: &RelationHandle, epoch: u64) -> Result<SeedPlan> {
    let (tokens, docs) = tx.scan_fts_doc_stats(idx)?;
    let records = tx.read_fts_records(idx, epoch)?;
    Ok(SeedPlan {
        generation: records.base.map_or(0, |b| b.generation),
        base_raw: records.base_raw,
        tokens,
        docs,
        delta_keys: records.delta_keys,
    })
}

/// Write half of a seed. Returns `false` when another seed changed the base
/// since the plan was taken; the caller may retry.
pub(crate) fn apply_seed(
    tx: &mut SessionTx<'_>,
    idx: &RelationHandle,
    epoch: u64,
    plan: SeedPlan,
) -> Result<bool> {
    let key = base_key(idx);
    // Read for update: locks the base key and validates it against this
    // transaction's snapshot, so two seeds cannot both commit.
    let current = tx.store_tx.get(&key, true)?;
    if current != plan.base_raw {
        return Ok(false);
    }
    let base = Base {
        epoch,
        generation: plan.generation.wrapping_add(1),
        tokens: plan.tokens,
        docs: plan.docs,
    };
    tx.store_tx.put(&key, &encode_base(base))?;
    for k in plan.delta_keys {
        tx.store_tx.del(&k)?;
    }
    Ok(true)
}

/// Number of delta records of `idx` visible to `tx`, whatever their epoch.
pub(crate) fn count_fts_deltas(
    tx: &SessionTx<'_>,
    idx: &RelationHandle,
    epoch: u64,
) -> Result<usize> {
    Ok(tx.read_fts_records(idx, epoch)?.delta_keys.len())
}

/// Read-only half of a fold: the base plus the deltas in one snapshot. `None`
/// when the index is unseeded, holds fewer than `min_deltas` deltas, or its
/// records fail the sanity guard (a seed or repair fixes those).
pub(crate) fn plan_fold(
    tx: &SessionTx<'_>,
    idx: &RelationHandle,
    epoch: u64,
    min_deltas: usize,
) -> Result<Option<SeedPlan>> {
    let records = tx.read_fts_records(idx, epoch)?;
    let Some(base) = records.base.filter(|b| b.epoch == epoch) else {
        return Ok(None);
    };
    if records.malformed || records.delta_keys.is_empty() || records.delta_keys.len() < min_deltas {
        return Ok(None);
    }
    let tokens = i128::from(base.tokens) + records.delta_tokens;
    let docs = i128::from(base.docs) + records.delta_docs;
    if tokens < 0 || docs < 0 || (tokens == 0) != (docs == 0) || tokens > i128::from(u64::MAX) {
        log::warn!(
            "fts.persisted_stats_invalid: not folding index {}: totals would be tokens {tokens}, \
             documents {docs}; run repair_fts_stats",
            idx.name
        );
        return Ok(None);
    }
    Ok(Some(SeedPlan {
        base_raw: records.base_raw,
        generation: base.generation,
        tokens: tokens as u64,
        docs: docs as u64,
        delta_keys: records.delta_keys,
    }))
}

/// Pending per-index change of one write transaction: the index, tokens, docs.
pub(crate) type FtsPending = BTreeMap<RelationId, (RelationHandle, i64, i64)>;

#[cfg(all(test, feature = "storage-rocksdb"))]
mod tests {
    use super::{base_key, encode_base, list_fts_indexes, Base};
    use std::collections::BTreeMap;

    #[test]
    fn repair_rewrites_plausible_but_wrong_totals() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::new_cozo_rocksdb(dir.path().join("db")).unwrap();
        db.enable_persisted_fts_stats().unwrap();
        let run = |s: &str| {
            db.run_script(s, BTreeMap::new(), crate::ScriptMutability::Mutable)
                .unwrap()
        };
        run(":create doc {id: String => body: String}");
        run("::fts create doc:idx {extractor: body, tokenizer: Simple, filters: [Lowercase]}");
        run("?[id, body] <- [['a', 'alpha beta'], ['b', 'alpha']] :put doc {id => body}");
        assert_eq!(db.seed_fts_stats().unwrap(), 1);
        let epoch = db.fts_persist.epoch().unwrap();

        // Overwrite the base with positive, plausible, wrong totals.
        let idx = {
            let tx = db.transact().unwrap();
            list_fts_indexes(&tx).unwrap().remove(0)
        };
        let mut tx = db.transact_write().unwrap();
        let wrong = Base {
            epoch,
            generation: 7,
            tokens: 999,
            docs: 9,
        };
        tx.store_tx
            .put(&base_key(&idx), &encode_base(wrong))
            .unwrap();
        tx.commit_tx().unwrap();
        let check = db.verify_fts_stats().unwrap().remove(0);
        assert_eq!(check.persisted, Some((999, 9)));

        assert_eq!(db.repair_fts_stats().unwrap(), vec!["doc:idx".to_string()]);
        let check = db.verify_fts_stats().unwrap().remove(0);
        assert_eq!(check.persisted, Some(check.scanned));
        assert_eq!(check.scanned, (3, 2));
        assert!(db.repair_fts_stats().unwrap().is_empty());
    }
}
