/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};

use miette::{bail, Result};
use smartstring::{LazyCompact, SmartString};
use crate::data::program::ReturnMutation;

use crate::data::tuple::TupleT;
use crate::data::value::DataValue;
use crate::fts::TokenizerCache;
use crate::{CallbackOp, NamedRows};
use crate::runtime::callback::CallbackCollector;
use crate::runtime::relation::RelationId;
use crate::runtime::tt_clock::{tt_hwm_key, TtClock};
use crate::storage::temp::TempTx;
use crate::storage::StoreTx;

pub struct SessionTx<'a> {
    pub(crate) store_tx: Box<dyn StoreTx<'a> + 'a>,
    pub(crate) temp_store_tx: TempTx,
    pub(crate) relation_store_id: Arc<AtomicU64>,
    pub(crate) temp_store_id: AtomicU32,
    pub(crate) tokenizers: Arc<TokenizerCache>,
    /// Cross-query cache of FTS corpus stats for legacy indexes (mnestic fork);
    /// see `Db::fts_doc_stats_cache`. Shared `Arc` cloned per transaction.
    pub(crate) fts_doc_stats_cache: Arc<Mutex<BTreeMap<SmartString<LazyCompact>, (u64, u64)>>>,
    /// Transaction-time commit clock (mnestic fork, bitemporality step 2);
    /// shared handle to the `Db`'s high-water mark. See `runtime/tt_clock.rs`.
    pub(crate) tt_clock: Arc<TtClock>,
    /// Per-`Db` critical section serialising tt allocation + HWM persist +
    /// commit for tt-stamped transactions (`docs/specs/bitemporality.md`
    /// §13.10). Held only inside `commit_tx_with_tt`.
    pub(crate) tt_commit_lock: Arc<Mutex<()>>,
}

pub const CURRENT_STORAGE_VERSION: [u8; 1] = [0x00];

fn storage_version_key() -> Vec<u8> {
    let storage_version_tuple = vec![DataValue::Null, DataValue::from("STORAGE_VERSION")];
    storage_version_tuple.encode_as_key(RelationId::SYSTEM)
}

const STATUS_STR: &str = "status";
const OK_STR: &str = "OK";

impl<'a> SessionTx<'a> {
    pub(crate) fn get_returning_rows(&self, callback_collector: &mut CallbackCollector, rel: &str, returning: &ReturnMutation) -> Result<NamedRows> {
        let returned_rows = {
            match returning {
                ReturnMutation::NotReturning => {
                    NamedRows::new(
                        vec![STATUS_STR.to_string()],
                        vec![vec![DataValue::from(OK_STR)]],
                    )
                }
                ReturnMutation::Returning => {
                    let meta = self.get_relation(rel, false)?;
                    let target_len = meta.metadata.keys.len() + meta.metadata.non_keys.len();
                    let mut returned_rows = Vec::new();
                    if let Some(collected) = callback_collector.get(&meta.name) {
                        for (kind, insertions, deletions) in collected {
                            let (pos_key, neg_key) = match kind {
                                CallbackOp::Put => { ("inserted", "replaced") }
                                CallbackOp::Rm => { ("requested", "deleted") }
                            };
                            for row in &insertions.rows {
                                let mut v = Vec::with_capacity(target_len + 1);
                                v.push(DataValue::from(pos_key));
                                v.extend_from_slice(row);
                                while v.len() <= target_len {
                                    v.push(DataValue::Null);
                                }
                                returned_rows.push(v);
                            }
                            for row in &deletions.rows {
                                let mut v = Vec::with_capacity(target_len + 1);
                                v.push(DataValue::from(neg_key));
                                v.extend_from_slice(row);
                                while v.len() <= target_len {
                                    v.push(DataValue::Null);
                                }
                                returned_rows.push(v);
                            }
                        }
                    }
                    let mut header = vec!["_kind".to_string()];
                    header.extend(meta.metadata.keys
                        .iter()
                        .chain(meta.metadata.non_keys.iter())
                        .map(|s| s.name.to_string()));
                    NamedRows::new(
                        header,
                        returned_rows,
                    )
                }
            }
        };
        Ok(returned_rows)
    }

    pub(crate) fn init_storage(&mut self) -> Result<RelationId> {
        let tuple = vec![DataValue::Null];
        let t_encoded = tuple.encode_as_key(RelationId::SYSTEM);
        let found = self.store_tx.get(&t_encoded, false)?;
        let storage_version_key = storage_version_key();
        let ret = match found {
            None => {
                self.store_tx
                    .put(&storage_version_key, &CURRENT_STORAGE_VERSION)?;
                self.store_tx
                    .put(&t_encoded, &RelationId::new(0).raw_encode())?;
                RelationId::SYSTEM
            }
            Some(slice) => {
                let version_found = self.store_tx.get(&storage_version_key, false)?;
                match version_found {
                    None => {
                        bail!("Storage is used but un-versioned, probably created by an ancient version of Cozo.")
                    }
                    Some(v) => {
                        if v != CURRENT_STORAGE_VERSION {
                            bail!(
                                "Version mismatch: expect storage version {:?}, got {:?}",
                                CURRENT_STORAGE_VERSION,
                                v
                            )
                        }
                    }
                }
                RelationId::raw_decode(&slice)
            }
        };
        Ok(ret)
    }

    pub fn commit_tx(&mut self) -> Result<()> {
        self.store_tx.commit()?;
        Ok(())
    }

    /// Read the persisted tt high-water mark, if any (mnestic fork,
    /// bitemporality step 2). Called at `Db` open to seed the clock. A
    /// malformed value is a **loud** error (mirrors `init_storage`'s
    /// version-mismatch bail): silently falling back to the wall clock would
    /// re-issue committed tts after a backward clock step — the exact
    /// failure the persisted mark exists to prevent. Future values are
    /// accepted (forward clock skew on a previous host is legitimate;
    /// monotonicity is the invariant, not plausibility).
    pub(crate) fn read_persisted_tt_hwm(&self) -> Result<Option<i64>> {
        match self.store_tx.get(&tt_hwm_key(), false)? {
            None => Ok(None),
            Some(v) => {
                let bytes: [u8; 8] = v.as_slice().try_into().map_err(|_| {
                    miette::miette!(
                        "tt high-water mark is corrupt: expected 8 bytes, got {} — \
                         refusing to open with a possibly non-monotonic transaction clock",
                        v.len()
                    )
                })?;
                let val = i64::from_be_bytes(bytes);
                if val < 0 {
                    bail!(
                        "tt high-water mark is corrupt (negative: {val}) — \
                         refusing to open with a possibly non-monotonic transaction clock"
                    );
                }
                Ok(Some(val))
            }
        }
    }

    /// Commit this transaction with an engine-assigned transaction time
    /// (mnestic fork, bitemporality step 2; spec §5/§13.10). Under the
    /// per-`Db` critical section: allocate the tt, persist the high-water
    /// mark **inside this same storage transaction** (so a crash can never
    /// leave the persisted mark behind a committed tt), then commit. Returns
    /// the allocated tt. tt order == commit order == visibility order by
    /// construction.
    ///
    /// Nothing calls this in production yet — step 3 (schema opt-in +
    /// buffered stamping) will route commits of transactions that touched
    /// tt-stamped relations through here, stamping the buffered rows with
    /// the returned tt before `store_tx.commit()`.
    #[allow(dead_code)]
    pub(crate) fn commit_tx_with_tt(&mut self) -> Result<i64> {
        let lock = self.tt_commit_lock.clone();
        // Poison recovery is sound here: a panic inside the section can only
        // have advanced the atomic (a burned, never-committed value) — the
        // invariant "atomic HWM >= persisted HWM >= every committed tt"
        // survives, so later committers may proceed rather than fail-stop.
        let _guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tt = self.tt_clock.advance();
        // put_externally_serialized: the mutex above is the serialization
        // authority for this key; RocksDB must not snapshot-validate it (two
        // overlapping tt commits would otherwise abort the later one).
        self.store_tx
            .put_externally_serialized(&tt_hwm_key(), &tt.to_be_bytes())?;
        self.store_tx.commit()?;
        Ok(tt)
    }
}
