/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::BTreeMap;
use std::fmt::{Debug, Display, Formatter};
use std::sync::atomic::Ordering;

use itertools::Itertools;
use log::error;
use miette::{bail, ensure, Diagnostic, IntoDiagnostic, Result};
use pest::Parser;
use rmp_serde::Serializer;
use serde::Serialize;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Bytecode;
use crate::data::memcmp::MemCmpEncoder;
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::symb::Symbol;
use crate::data::tuple::{decode_tuple_from_key, Tuple, TupleT, ENCODED_KEY_MIN_LEN};
use crate::data::value::{DataValue, ValidityTs};
use crate::fts::indexing::encode_fts_rows_for_tuple;
use crate::fts::FtsIndexManifest;
use crate::parse::expr::build_expr;
use crate::parse::sys::{FtsIndexConfig, HnswIndexConfig, MinHashLshConfig};
use crate::parse::{CozoScriptParser, Rule, SourceSpan};
use crate::query::compile::IndexPositionUse;
use crate::runtime::hnsw::HnswIndexManifest;
use crate::runtime::minhash_lsh::{HashPermutations, LshParams, MinHashLshIndexManifest, Weights};
use crate::runtime::transact::SessionTx;
use crate::utils::TempCollector;
use crate::{NamedRows, StoreTx};

#[derive(
    Copy,
    Clone,
    Eq,
    PartialEq,
    Debug,
    serde_derive::Serialize,
    serde_derive::Deserialize,
    PartialOrd,
    Ord,
)]
pub(crate) struct RelationId(pub(crate) u64);

impl RelationId {
    pub(crate) fn new(u: u64) -> Self {
        if u > 2u64.pow(6 * 8) {
            panic!("StoredRelId overflow: {u}")
        } else {
            Self(u)
        }
    }
    pub(crate) fn next(&self) -> Self {
        Self::new(self.0 + 1)
    }
    pub(crate) const SYSTEM: Self = Self(0);
    pub(crate) fn raw_encode(&self) -> [u8; 8] {
        self.0.to_be_bytes()
    }
    pub(crate) fn raw_decode(src: &[u8]) -> Self {
        let u = u64::from_be_bytes([
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ]);
        Self::new(u)
    }
}

#[derive(Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct RelationHandle {
    pub(crate) name: SmartString<LazyCompact>,
    pub(crate) id: RelationId,
    pub(crate) metadata: StoredRelationMetadata,
    pub(crate) put_triggers: Vec<String>,
    pub(crate) rm_triggers: Vec<String>,
    pub(crate) replace_triggers: Vec<String>,
    pub(crate) access_level: AccessLevel,
    pub(crate) is_temp: bool,
    pub(crate) indices: BTreeMap<SmartString<LazyCompact>, (RelationHandle, Vec<usize>)>,
    pub(crate) hnsw_indices:
        BTreeMap<SmartString<LazyCompact>, (RelationHandle, HnswIndexManifest)>,
    pub(crate) fts_indices: BTreeMap<SmartString<LazyCompact>, (RelationHandle, FtsIndexManifest)>,
    pub(crate) lsh_indices: BTreeMap<
        SmartString<LazyCompact>,
        (RelationHandle, RelationHandle, MinHashLshIndexManifest),
    >,
    pub(crate) description: SmartString<LazyCompact>,
    /// mnestic fork, bitemporality step 5: after `::history_gc rel cutoff`,
    /// as-of reads below the cutoff would silently return a post-hoc
    /// reconstruction, so they error instead. `None` = never garbage
    /// collected.
    ///
    /// MUST stay the LAST field. rmp_serde encodes structs as positional
    /// arrays on the pre-`with_struct_map` catalog-write paths, and
    /// `#[serde(default)]` only rescues a *missing trailing* element. A
    /// mid-struct position here bricks every legacy (13-field) catalog on
    /// upgrade — see the `legacy_catalog_without_tt_gc_floor` regression test.
    #[serde(default)]
    pub(crate) tt_gc_floor: Option<i64>,
}

impl RelationHandle {
    pub(crate) fn has_index(&self, index_name: &str) -> bool {
        self.indices.contains_key(index_name)
            || self.hnsw_indices.contains_key(index_name)
            || self.fts_indices.contains_key(index_name)
            || self.lsh_indices.contains_key(index_name)
    }
    pub(crate) fn has_no_index(&self) -> bool {
        self.indices.is_empty()
            && self.hnsw_indices.is_empty()
            && self.fts_indices.is_empty()
            && self.lsh_indices.is_empty()
    }
}

#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    PartialEq,
    serde_derive::Serialize,
    serde_derive::Deserialize,
    Default,
    Ord,
    PartialOrd,
)]
pub enum AccessLevel {
    Hidden,
    ReadOnly,
    Protected,
    #[default]
    Normal,
}

impl Display for AccessLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessLevel::Normal => f.write_str("normal"),
            AccessLevel::Protected => f.write_str("protected"),
            AccessLevel::ReadOnly => f.write_str("read_only"),
            AccessLevel::Hidden => f.write_str("hidden"),
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Arity mismatch for stored relation {name}: expect {expect_arity}, got {actual_arity}")]
#[diagnostic(code(eval::stored_rel_arity_mismatch))]
struct StoredRelArityMismatch {
    name: String,
    expect_arity: usize,
    actual_arity: usize,
    #[label]
    span: SourceSpan,
}

impl RelationHandle {
    pub(crate) fn raw_binding_map(&self) -> BTreeMap<Symbol, usize> {
        let mut ret = BTreeMap::new();
        for (i, col) in self.metadata.keys.iter().enumerate() {
            ret.insert(Symbol::new(col.name.clone(), Default::default()), i);
        }
        for (i, col) in self.metadata.non_keys.iter().enumerate() {
            ret.insert(
                Symbol::new(col.name.clone(), Default::default()),
                i + self.metadata.keys.len(),
            );
        }
        ret
    }
    pub(crate) fn has_triggers(&self) -> bool {
        !self.put_triggers.is_empty() || !self.rm_triggers.is_empty()
    }
    fn encode_key_prefix(&self, len: usize) -> Vec<u8> {
        let mut ret = Vec::with_capacity(4 + 4 * len + 10 * len);
        let prefix_bytes = self.id.0.to_be_bytes();
        ret.extend(prefix_bytes);
        ret
    }
    pub(crate) fn as_named_rows(&self, tx: &SessionTx<'_>) -> Result<NamedRows> {
        let rows: Vec<_> = self.scan_all(tx).try_collect()?;
        let mut headers = self
            .metadata
            .keys
            .iter()
            .map(|col| col.name.to_string())
            .collect_vec();
        headers.extend(
            self.metadata
                .non_keys
                .iter()
                .map(|col| col.name.to_string()),
        );
        Ok(NamedRows::new(headers, rows))
    }
    #[allow(dead_code)]
    pub(crate) fn amend_key_prefix(&self, data: &mut [u8]) {
        let prefix_bytes = self.id.0.to_be_bytes();
        data[0..8].copy_from_slice(&prefix_bytes);
    }
    pub(crate) fn choose_index(
        &self,
        arg_uses: &[IndexPositionUse],
        validity_query: bool,
    ) -> Option<(RelationHandle, Vec<usize>, bool)> {
        if self.indices.is_empty() {
            return None;
        }
        if *arg_uses.first().unwrap() == IndexPositionUse::Join {
            return None;
        }
        let mut max_prefix_len = 0;
        let required_positions = arg_uses
            .iter()
            .enumerate()
            .filter_map(|(i, pos_use)| {
                if *pos_use != IndexPositionUse::Ignored {
                    Some(i)
                } else {
                    None
                }
            })
            .collect_vec();
        let mut chosen = None;
        for (manifest, mapper) in self.indices.values() {
            if validity_query && *mapper.last().unwrap() != self.metadata.keys.len() - 1 {
                continue;
            }

            let mut cur_prefix_len = 0;
            for i in mapper {
                if arg_uses[*i] == IndexPositionUse::Join {
                    cur_prefix_len += 1;
                } else {
                    break;
                }
            }
            if cur_prefix_len > max_prefix_len {
                max_prefix_len = cur_prefix_len;
                let mut need_join = false;
                for need_pos in required_positions.iter() {
                    if !mapper.contains(need_pos) {
                        need_join = true;
                        break;
                    }
                }
                chosen = Some((manifest.clone(), mapper.clone(), need_join))
            }
        }
        chosen
    }
    pub(crate) fn encode_key_for_store(
        &self,
        tuple: &[DataValue],
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let len = self.metadata.keys.len();
        ensure!(
            tuple.len() >= len,
            StoredRelArityMismatch {
                name: self.name.to_string(),
                expect_arity: self.arity(),
                actual_arity: tuple.len(),
                span
            }
        );
        let mut ret = self.encode_key_prefix(len);
        for val in &tuple[0..len] {
            ret.encode_datavalue(val);
        }
        Ok(ret)
    }
    pub(crate) fn encode_partial_key_for_store(&self, tuple: &[DataValue]) -> Vec<u8> {
        let mut ret = self.encode_key_prefix(tuple.len());
        for val in tuple {
            ret.encode_datavalue(val);
        }
        ret
    }
    pub(crate) fn encode_val_for_store(
        &self,
        tuple: &[DataValue],
        _span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let start = self.metadata.keys.len();
        let len = self.metadata.non_keys.len();
        let mut ret = self.encode_key_prefix(len);
        tuple[start..]
            .serialize(&mut Serializer::new(&mut ret))
            .unwrap();
        Ok(ret)
    }
    pub(crate) fn encode_val_only_for_store(
        &self,
        tuple: &[DataValue],
        _span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let mut ret = self.encode_key_prefix(tuple.len());
        tuple.serialize(&mut Serializer::new(&mut ret)).unwrap();
        Ok(ret)
    }
    pub(crate) fn ensure_compatible(
        &self,
        inp: &InputRelationHandle,
        op: crate::data::program::RelationOp,
    ) -> Result<()> {
        use crate::data::program::RelationOp;
        let is_remove_or_update =
            matches!(op, RelationOp::Rm | RelationOp::Delete | RelationOp::Update);
        let InputRelationHandle { metadata, .. } = inp;
        // check that every given key is found and compatible
        for col in metadata.keys.iter().chain(self.metadata.non_keys.iter()) {
            self.metadata.compatible_with_col(col)?
        }
        // check that every key is provided or has default
        let n = self.metadata.keys.len();
        for (i, col) in self.metadata.keys.iter().enumerate() {
            // mnestic fork, bitemporality 4c: on a bitemporal relation the
            // ops that target the CURRENT belief (:update, :ensure,
            // :ensure_not) must not bind the vt column — the engine resolves
            // it — so it is not required of their input.
            if self.has_txtime()
                && !self.is_tt_only()
                && i == n - 2
                && matches!(
                    op,
                    RelationOp::Update | RelationOp::Ensure | RelationOp::EnsureNot
                )
            {
                continue;
            }
            metadata.satisfied_by_required_col(col)?;
        }
        if !is_remove_or_update {
            for col in &self.metadata.non_keys {
                metadata.satisfied_by_required_col(col)?;
            }
        }
        Ok(())
    }
}

/// Enforce the temporal-axis rule at `:create` time (mnestic fork,
/// bitemporality step 3; `docs/specs/bitemporality.md` §4/§13.1): temporal
/// axes are the trailing key columns in the fixed order vt-then-tt, at most
/// one of each. `TxTime` is new (no shipped uses), so malformed declarations
/// fail HERE, loudly, with the corrected declaration in the message —
/// deliberately stricter than `Validity`'s shipped query-time-only check.
fn validate_temporal_axes(input_meta: &InputRelationHandle) -> Result<()> {
    use crate::data::relation::{ColType, ColumnDef};

    let keys = &input_meta.metadata.keys;
    let non_keys = &input_meta.metadata.non_keys;
    let is_tt = |c: &&ColumnDef| matches!(c.typing.coltype, ColType::TxTime);
    let is_vt = |c: &&ColumnDef| matches!(c.typing.coltype, ColType::Validity);

    let tt_in_keys = keys.iter().filter(is_tt).count();
    let tt_in_vals = non_keys.iter().filter(is_tt).count();
    if tt_in_keys == 0 && tt_in_vals == 0 {
        return Ok(());
    }

    // The copy-pasteable corrected declaration: non-temporal keys in declared
    // order, then the (single) Validity, then the (single) TxTime, then the
    // non-temporal value columns. Defaults are omitted from the rendering.
    let corrected = {
        let plain_keys = keys
            .iter()
            .filter(|c| !is_tt(c) && !is_vt(c))
            .map(|c| format!("{}: {}", c.name, c.typing))
            .collect::<Vec<_>>();
        let vt = keys
            .iter()
            .chain(non_keys.iter())
            .find(|c| is_vt(c))
            .map(|c| format!("{}: {}", c.name, c.typing));
        let tt = keys
            .iter()
            .chain(non_keys.iter())
            .find(|c| is_tt(c))
            .map(|c| format!("{}: TxTime", c.name));
        let mut key_parts = plain_keys;
        key_parts.extend(vt);
        key_parts.extend(tt);
        let val_parts = non_keys
            .iter()
            .filter(|c| !is_tt(c))
            .map(|c| format!("{}: {}", c.name, c.typing))
            .collect::<Vec<_>>();
        if val_parts.is_empty() {
            format!(
                ":create {} {{{}}}",
                input_meta.name.name,
                key_parts.join(", ")
            )
        } else {
            format!(
                ":create {} {{{} => {}}}",
                input_meta.name.name,
                key_parts.join(", "),
                val_parts.join(", ")
            )
        }
    };

    #[derive(Debug, Error, Diagnostic)]
    #[error("invalid temporal-axis declaration: {reason}")]
    #[diagnostic(
        code(eval::invalid_temporal_axes),
        help("temporal axes must be the trailing key columns, in the order vt (Validity) then tt (TxTime), at most one of each; corrected declaration: `{corrected}`")
    )]
    struct InvalidTemporalAxes {
        reason: String,
        corrected: String,
        #[label]
        span: SourceSpan,
    }
    let err = |reason: &str| InvalidTemporalAxes {
        reason: reason.to_string(),
        corrected: corrected.clone(),
        span: input_meta.span,
    };

    if input_meta.name.is_temp_store_name() {
        bail!(err("TxTime is not supported on transaction-temp (`_`-prefixed) relations — temp stores have no commit clock"));
    }
    if tt_in_vals > 0 {
        bail!(err("TxTime must be a key column, not a value column"));
    }
    if tt_in_keys > 1 {
        bail!(err("at most one TxTime column is allowed"));
    }
    if keys
        .iter()
        .find(is_tt)
        .expect("tt_in_keys == 1")
        .typing
        .nullable
    {
        bail!(err(
            "TxTime cannot be nullable (it is engine-assigned at every commit)"
        ));
    }
    let vt_in_keys = keys.iter().filter(is_vt).count();
    if vt_in_keys > 1 {
        bail!(err(
            "at most one Validity column is allowed when TxTime is declared"
        ));
    }
    if keys.iter().any(|c| is_vt(&&c.clone()) && c.typing.nullable) {
        bail!(err(
            "the Validity axis cannot be nullable when TxTime is declared (the two-level resolution has no semantics for a null vt)"
        ));
    }
    let n = keys.len();
    if !matches!(keys[n - 1].typing.coltype, ColType::TxTime) {
        bail!(err("TxTime must be the last key column"));
    }
    if vt_in_keys == 1 && (n < 2 || !matches!(keys[n - 2].typing.coltype, ColType::Validity)) {
        bail!(err(
            "Validity must immediately precede TxTime (the vt-then-tt trailing pair)"
        ));
    }
    Ok(())
}

/// How a read resolves against a relation's temporal axes (mnestic fork).
#[derive(Debug, Clone, Copy)]
pub(crate) enum TemporalRead {
    /// no temporal machinery: plain scan
    Plain,
    /// single trailing-axis skip-scan at the point (vt relations with `@`,
    /// and tt-only relations — where the point is on the tt axis)
    AsOf(ValidityTs),
    /// two-level (vt, tt) resolution on a bitemporal relation
    Bitemporal {
        vt: Option<ValidityTs>,
        tt: ValidityTs,
    },
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct InputRelationHandle {
    pub(crate) name: Symbol,
    pub(crate) metadata: StoredRelationMetadata,
    pub(crate) key_bindings: Vec<Symbol>,
    pub(crate) dep_bindings: Vec<Symbol>,
    pub(crate) span: SourceSpan,
}

impl Debug for RelationHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Relation<{}>", self.name)
    }
}

#[derive(thiserror::Error, miette::Diagnostic, Debug)]
#[error("Cannot deserialize relation")]
#[diagnostic(code(deser::relation))]
#[diagnostic(help(
    "This could indicate a bug, or you are using an incompatible DB version. \
Consider file a bug report."
))]
pub(crate) struct RelationDeserError;

impl RelationHandle {
    /// Whether this relation is tt-stamped (its last key column is `TxTime`;
    /// mnestic fork, bitemporality). Guaranteed by `validate_temporal_axes`
    /// to be the only possible TxTime position.
    pub(crate) fn has_txtime(&self) -> bool {
        matches!(
            self.metadata.keys.last().map(|c| &c.typing.coltype),
            Some(crate::data::relation::ColType::TxTime)
        )
    }

    /// tt-only (system-versioned): tt-stamped with no vt axis.
    pub(crate) fn is_tt_only(&self) -> bool {
        self.has_txtime() && {
            let n = self.metadata.keys.len();
            n < 2
                || !matches!(
                    self.metadata.keys[n - 2].typing.coltype,
                    crate::data::relation::ColType::Validity
                )
        }
    }

    /// After `::history_gc`, an as-of read below the persisted floor would
    /// silently return a post-hoc reconstruction as if it were the historical
    /// belief — error instead (mnestic fork, bitemporality step 5).
    fn check_tt_gc_floor(&self, tt: Option<ValidityTs>, span: SourceSpan) -> Result<()> {
        if let (Some(t), Some(floor)) = (tt, self.tt_gc_floor) {
            if t.0 .0 < floor {
                #[derive(Debug, Error, Diagnostic)]
                #[error("as-of read below the ::history_gc floor of relation {0} ({1} < {2})")]
                #[diagnostic(
                    code(eval::txtime_below_gc_floor),
                    help("records below the floor were garbage-collected; the belief at that time is no longer reconstructible")
                )]
                struct BelowGcFloor(String, i64, i64, #[label] SourceSpan);
                bail!(BelowGcFloor(self.name.to_string(), t.0 .0, floor, span));
            }
        }
        Ok(())
    }

    /// Resolve a read's temporal selectors against this relation's axes
    /// (mnestic fork, bitemporality steps 4a/4b; spec §4 semantics table).
    /// On tt-only relations the tt axis DEFAULTS to end-of-tt-time — the
    /// default read is the current state, so adding a `tt: TxTime` column
    /// changes no existing query's results (the migration invariant); only an
    /// explicit `@ (tt: …)` reaches history. On bitemporal relations the vt
    /// axis keeps its shipped semantics (no selector = every vt record,
    /// resolved to the belief at T) and the tt axis defaults to current
    /// belief — the same invariant, two axes.
    pub(crate) fn resolve_temporal_read(
        &self,
        vt: Option<ValidityTs>,
        tt: Option<ValidityTs>,
        span: SourceSpan,
    ) -> Result<TemporalRead> {
        use crate::data::functions::MAX_VALIDITY_TS;
        if self.has_txtime() {
            if self.is_tt_only() {
                if vt.is_some() {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("relation {0} is system-versioned: it has no valid-time axis")]
                    #[diagnostic(
                        code(eval::txtime_no_vt_axis),
                        help("select transaction time with `@ (tt: …)`; bare `@ E` always means valid time")
                    )]
                    struct NoVtAxis(String, #[label] SourceSpan);
                    bail!(NoVtAxis(self.name.to_string(), span));
                }
                self.check_tt_gc_floor(tt, span)?;
                Ok(TemporalRead::AsOf(tt.unwrap_or(MAX_VALIDITY_TS)))
            } else {
                // Bitemporal (step 4b): the two-level resolution. No vt
                // selector = every vt record (resolve-groups); a vt selector
                // = the single belief per key (resolve-key). tt defaults to
                // current belief.
                self.check_tt_gc_floor(tt, span)?;
                Ok(TemporalRead::Bitemporal {
                    vt,
                    tt: tt.unwrap_or(MAX_VALIDITY_TS),
                })
            }
        } else {
            if tt.is_some() {
                #[derive(Debug, Error, Diagnostic)]
                #[error("relation {0} has no transaction-time axis")]
                #[diagnostic(
                    code(eval::txtime_no_tt_axis),
                    help("declare a trailing `tt: TxTime` key column to make the relation transaction-time-stamped")
                )]
                struct NoTtAxis(String, #[label] SourceSpan);
                bail!(NoTtAxis(self.name.to_string(), span));
            }
            match vt {
                None => Ok(TemporalRead::Plain),
                Some(v) => {
                    if self.metadata.keys.last().map(|c| &c.typing.coltype)
                        != Some(&crate::data::relation::ColType::Validity)
                        || self.metadata.keys.last().unwrap().typing.nullable
                    {
                        bail!(crate::query::ra::InvalidTimeTravelScanning(
                            self.name.to_string(),
                            span
                        ));
                    }
                    Ok(TemporalRead::AsOf(v))
                }
            }
        }
    }

    pub(crate) fn arity(&self) -> usize {
        self.metadata.non_keys.len() + self.metadata.keys.len()
    }
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        Ok(rmp_serde::from_slice(data).map_err(|e| {
            error!(
                "Cannot deserialize relation metadata from bytes: {:x?}, {:?}",
                data, e
            );
            RelationDeserError
        })?)
    }
    pub(crate) fn scan_all<'a>(
        &self,
        tx: &'a SessionTx<'_>,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let lower = Tuple::default().encode_as_key(self.id);
        let upper = Tuple::default().encode_as_key(self.id.next());
        if self.is_temp {
            tx.temp_store_tx.range_scan_tuple(&lower, &upper)
        } else {
            tx.store_tx.range_scan_tuple(&lower, &upper)
        }
    }

    pub(crate) fn skip_scan_all<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        valid_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let lower = Tuple::default().encode_as_key(self.id);
        let upper = Tuple::default().encode_as_key(self.id.next());
        if self.is_temp {
            tx.temp_store_tx
                .range_skip_scan_tuple(&lower, &upper, valid_at)
        } else {
            tx.store_tx.range_skip_scan_tuple(&lower, &upper, valid_at)
        }
    }

    pub(crate) fn get(&self, tx: &SessionTx<'_>, key: &[DataValue]) -> Result<Option<Tuple>> {
        let key_data = key.encode_as_key(self.id);
        if self.is_temp {
            Ok(tx
                .temp_store_tx
                .get(&key_data, false)?
                .map(|val_data| decode_tuple_from_kv(&key_data, &val_data, Some(self.arity()))))
        } else {
            Ok(tx
                .store_tx
                .get(&key_data, false)?
                .map(|val_data| decode_tuple_from_kv(&key_data, &val_data, Some(self.arity()))))
        }
    }

    /// Batched point lookups (mnestic fork): encode all keys and serve them
    /// through one `StoreTx::multi_get` — a true RocksDB `MultiGet` on the
    /// snapshot read path (shared filter probes, batched block reads).
    /// Returns one entry per key, in order.
    pub(crate) fn get_batch(
        &self,
        tx: &SessionTx<'_>,
        keys: &[&[DataValue]],
    ) -> Result<Vec<Option<Tuple>>> {
        let encoded: Vec<Vec<u8>> = keys.iter().map(|k| k.encode_as_key(self.id)).collect();
        let raw = if self.is_temp {
            tx.temp_store_tx.multi_get(&encoded, false)?
        } else {
            tx.store_tx.multi_get(&encoded, false)?
        };
        Ok(raw
            .into_iter()
            .zip(encoded.iter())
            .map(|(v, k)| v.map(|val| decode_tuple_from_kv(k, &val, Some(self.arity()))))
            .collect())
    }

    pub(crate) fn get_val_only(
        &self,
        tx: &SessionTx<'_>,
        key: &[DataValue],
    ) -> Result<Option<Tuple>> {
        let key_data = key.encode_as_key(self.id);
        if self.is_temp {
            Ok(tx
                .temp_store_tx
                .get(&key_data, false)?
                .map(|val_data| rmp_serde::from_slice(&val_data[ENCODED_KEY_MIN_LEN..]).unwrap()))
        } else {
            Ok(tx
                .store_tx
                .get(&key_data, false)?
                .map(|val_data| rmp_serde::from_slice(&val_data[ENCODED_KEY_MIN_LEN..]).unwrap()))
        }
    }

    pub(crate) fn exists(&self, tx: &SessionTx<'_>, key: &[DataValue]) -> Result<bool> {
        let key_data = key.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx.exists(&key_data, false)
        } else {
            tx.store_tx.exists(&key_data, false)
        }
    }

    pub(crate) fn scan_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &Tuple,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower = prefix.clone();
        lower.truncate(self.metadata.keys.len());
        let mut upper = lower.clone();
        upper.push(DataValue::Bot);
        let prefix_encoded = lower.encode_as_key(self.id);
        let upper_encoded = upper.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .range_scan_tuple(&prefix_encoded, &upper_encoded)
        } else {
            tx.store_tx
                .range_scan_tuple(&prefix_encoded, &upper_encoded)
        }
    }

    /// Two-level bitemporal scans (mnestic fork, step 4b): whole relation.
    pub(crate) fn bitemporal_scan_all<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let lower = Tuple::default().encode_as_key(self.id);
        let upper = Tuple::default().encode_as_key(self.id.next());
        tx.store_tx
            .range_bitemporal_scan_tuple(&lower, &upper, vt_at, tt_at)
    }

    /// Two-level bitemporal scan narrowed to a key prefix (step 4b).
    pub(crate) fn bitemporal_scan_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &Tuple,
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower = prefix.clone();
        lower.truncate(self.metadata.keys.len());
        let mut upper = lower.clone();
        upper.push(DataValue::Bot);
        let prefix_encoded = lower.encode_as_key(self.id);
        let upper_encoded = upper.encode_as_key(self.id);
        tx.store_tx
            .range_bitemporal_scan_tuple(&prefix_encoded, &upper_encoded, vt_at, tt_at)
    }

    pub(crate) fn skip_scan_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &Tuple,
        valid_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower = prefix.clone();
        lower.truncate(self.metadata.keys.len());
        let mut upper = lower.clone();
        upper.push(DataValue::Bot);
        let prefix_encoded = lower.encode_as_key(self.id);
        let upper_encoded = upper.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .range_skip_scan_tuple(&prefix_encoded, &upper_encoded, valid_at)
        } else {
            tx.store_tx
                .range_skip_scan_tuple(&prefix_encoded, &upper_encoded, valid_at)
        }
    }

    pub(crate) fn scan_bounded_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &[DataValue],
        lower: &[DataValue],
        upper: &[DataValue],
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower_t = prefix.to_vec();
        lower_t.extend_from_slice(lower);
        let mut upper_t = prefix.to_vec();
        upper_t.extend_from_slice(upper);
        upper_t.push(DataValue::Bot);
        let lower_encoded = lower_t.encode_as_key(self.id);
        let upper_encoded = upper_t.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .range_scan_tuple(&lower_encoded, &upper_encoded)
        } else {
            tx.store_tx.range_scan_tuple(&lower_encoded, &upper_encoded)
        }
    }
    pub(crate) fn skip_scan_bounded_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &Tuple,
        lower: &[DataValue],
        upper: &[DataValue],
        valid_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower_t = prefix.clone();
        lower_t.extend_from_slice(lower);
        let mut upper_t = prefix.clone();
        upper_t.extend_from_slice(upper);
        upper_t.push(DataValue::Bot);
        let lower_encoded = lower_t.encode_as_key(self.id);
        let upper_encoded = upper_t.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .range_skip_scan_tuple(&lower_encoded, &upper_encoded, valid_at)
        } else {
            tx.store_tx
                .range_skip_scan_tuple(&lower_encoded, &upper_encoded, valid_at)
        }
    }
}

const DEFAULT_SIZE_HINT: usize = 16;

/// Decode tuple from key-value pairs. Used for customizing storage
/// in trait [`StoreTx`](crate::StoreTx).
#[inline]
pub fn decode_tuple_from_kv(key: &[u8], val: &[u8], size_hint: Option<usize>) -> Tuple {
    let mut tup = decode_tuple_from_key(key, size_hint.unwrap_or(DEFAULT_SIZE_HINT));
    extend_tuple_from_v(&mut tup, val);
    tup
}

pub fn extend_tuple_from_v(key: &mut Tuple, val: &[u8]) {
    if !val.is_empty() {
        let vals: Vec<DataValue> = rmp_serde::from_slice(&val[ENCODED_KEY_MIN_LEN..]).unwrap();
        key.extend(vals);
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("index {0} for relation {1} already exists")]
#[diagnostic(code(tx::index_already_exists))]
pub(crate) struct IndexAlreadyExists(String, String);

#[derive(Debug, Diagnostic, Error)]
#[error("Cannot create relation {0} as one with the same name already exists")]
#[diagnostic(code(eval::rel_name_conflict))]
struct RelNameConflictError(String);

impl<'a> SessionTx<'a> {
    pub(crate) fn relation_exists(&self, name: &str) -> Result<bool> {
        let key = DataValue::from(name);
        let encoded = vec![key].encode_as_key(RelationId::SYSTEM);
        if name.starts_with('_') {
            self.temp_store_tx.exists(&encoded, false)
        } else {
            self.store_tx.exists(&encoded, false)
        }
    }
    /// Bail if `rel` is tt-stamped: triggers and secondary/search indexes are
    /// not yet supported on TxTime relations (mnestic fork, bitemporality
    /// step 3 — buffered commit-time stamping is incompatible with
    /// statement-time trigger/index maintenance; B-tree index support is
    /// step 5, `docs/specs/bitemporality.md` §8).
    fn reject_txtime_relation(rel: &RelationHandle, what: &str) -> Result<()> {
        if rel.has_txtime() {
            #[derive(Debug, Error, Diagnostic)]
            #[error("{0} is not supported on TxTime (transaction-time) relations: {1} (statement-time index/trigger maintenance is incompatible with buffered commit-time stamping; revisit on real pull)")]
            #[diagnostic(code(eval::txtime_unsupported_op))]
            struct TxTimeUnsupported(String, String);
            bail!(TxTimeUnsupported(what.to_string(), rel.name.to_string()));
        }
        Ok(())
    }

    pub(crate) fn set_relation_triggers(
        &mut self,
        name: &Symbol,
        puts: &[String],
        rms: &[String],
        replaces: &[String],
    ) -> Result<()> {
        if name.name.starts_with('_') {
            bail!("Cannot set triggers for temp store")
        }
        let mut original = self.get_relation(name, true)?;
        Self::reject_txtime_relation(&original, "::set_triggers")?;
        if original.access_level < AccessLevel::Protected {
            bail!(InsufficientAccessLevel(
                original.name.to_string(),
                "set triggers".to_string(),
                original.access_level
            ))
        }
        original.put_triggers = puts.to_vec();
        original.rm_triggers = rms.to_vec();
        original.replace_triggers = replaces.to_vec();

        let name_key =
            vec![DataValue::Str(original.name.clone())].encode_as_key(RelationId::SYSTEM);

        let mut meta_val = vec![];
        original
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&name_key, &meta_val)?;

        Ok(())
    }
    pub(crate) fn create_relation(
        &mut self,
        input_meta: InputRelationHandle,
    ) -> Result<RelationHandle> {
        validate_temporal_axes(&input_meta)?;
        let key = DataValue::Str(input_meta.name.name.clone());
        let encoded = vec![key].encode_as_key(RelationId::SYSTEM);

        let is_temp = input_meta.name.is_temp_store_name();

        if is_temp {
            if self.store_tx.exists(&encoded, true)? {
                bail!(RelNameConflictError(input_meta.name.to_string()))
            };
        } else if self.temp_store_tx.exists(&encoded, true)? {
            bail!(RelNameConflictError(input_meta.name.to_string()))
        }

        let metadata = input_meta.metadata.clone();
        let last_id = if is_temp {
            self.temp_store_id.fetch_add(1, Ordering::Relaxed) as u64
        } else {
            self.relation_store_id.fetch_add(1, Ordering::SeqCst)
        };
        let meta = RelationHandle {
            name: input_meta.name.name,
            id: RelationId::new(last_id + 1),
            metadata,
            put_triggers: vec![],
            rm_triggers: vec![],
            replace_triggers: vec![],
            access_level: AccessLevel::Normal,
            is_temp,
            tt_gc_floor: None,
            indices: Default::default(),
            hnsw_indices: Default::default(),
            fts_indices: Default::default(),
            lsh_indices: Default::default(),
            description: Default::default(),
        };

        let name_key = vec![DataValue::Str(meta.name.clone())].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        meta.serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        let tuple = vec![DataValue::Null];
        let t_encoded = tuple.encode_as_key(RelationId::SYSTEM);

        if is_temp {
            self.temp_store_tx.put(&encoded, &meta.id.raw_encode())?;
            self.temp_store_tx.put(&name_key, &meta_val)?;
            self.temp_store_tx.put(&t_encoded, &meta.id.raw_encode())?;
        } else {
            self.store_tx.put(&encoded, &meta.id.raw_encode())?;
            self.store_tx.put(&name_key, &meta_val)?;
            self.store_tx.put(&t_encoded, &meta.id.raw_encode())?;
        }

        Ok(meta)
    }
    pub(crate) fn get_relation(&self, name: &str, lock: bool) -> Result<RelationHandle> {
        #[derive(Error, Diagnostic, Debug)]
        #[error("Cannot find requested stored relation '{0}'")]
        #[diagnostic(code(query::relation_not_found))]
        struct StoredRelationNotFoundError(String);

        let key = DataValue::from(name);
        let encoded = vec![key].encode_as_key(RelationId::SYSTEM);

        let found = if name.starts_with('_') {
            self.temp_store_tx
                .get(&encoded, lock)?
                .ok_or_else(|| StoredRelationNotFoundError(name.to_string()))?
        } else {
            self.store_tx
                .get(&encoded, lock)?
                .ok_or_else(|| StoredRelationNotFoundError(name.to_string()))?
        };
        let metadata = RelationHandle::decode(&found)?;
        Ok(metadata)
    }
    pub(crate) fn describe_relation(&mut self, name: &str, description: &str) -> Result<()> {
        let mut meta = self.get_relation(name, true)?;

        meta.description = SmartString::from(description);
        let name_key = vec![DataValue::Str(meta.name.clone())].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        meta.serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        if meta.is_temp {
            self.temp_store_tx.put(&name_key, &meta_val)?;
        } else {
            self.store_tx.put(&name_key, &meta_val)?;
        }

        Ok(())
    }
    pub(crate) fn destroy_relation(&mut self, name: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        if self
            .pending_tt_writes
            .iter()
            .any(|w| w.handle.name.as_str() == name as &str)
        {
            bail!(
                "relation {} has pending transaction-time writes in this transaction; \
                 commit them in their own transaction before removing the relation",
                name
            );
        }
        let is_temp = name.starts_with('_');
        let mut to_clean = vec![];

        // if name.starts_with('_') {
        //     bail!("Cannot destroy temp relation");
        // }
        let store = self.get_relation(name, true)?;
        if !store.has_no_index() {
            bail!(
                "Cannot remove stored relation `{}` with indices attached.",
                name
            );
        }
        if store.access_level < AccessLevel::Normal {
            bail!(InsufficientAccessLevel(
                store.name.to_string(),
                "relation removal".to_string(),
                store.access_level
            ))
        }

        for k in store.indices.keys() {
            let more_to_clean = self.destroy_relation(&format!("{name}:{k}"))?;
            to_clean.extend(more_to_clean);
        }

        for k in store.hnsw_indices.keys() {
            let more_to_clean = self.destroy_relation(&format!("{name}:{k}"))?;
            to_clean.extend(more_to_clean);
        }

        let key = DataValue::from(name);
        let encoded = vec![key].encode_as_key(RelationId::SYSTEM);
        if is_temp {
            self.temp_store_tx.del(&encoded)?;
        } else {
            self.store_tx.del(&encoded)?;
        }
        let lower_bound = Tuple::default().encode_as_key(store.id);
        let upper_bound = Tuple::default().encode_as_key(store.id.next());
        to_clean.push((lower_bound, upper_bound));
        Ok(to_clean)
    }
    pub(crate) fn set_access_level(&mut self, rel: &Symbol, level: AccessLevel) -> Result<()> {
        let mut meta = self.get_relation(rel, true)?;
        meta.access_level = level;

        let name_key = vec![DataValue::Str(meta.name.clone())].encode_as_key(RelationId::SYSTEM);

        let mut meta_val = vec![];
        meta.serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&name_key, &meta_val)?;

        Ok(())
    }

    pub(crate) fn create_minhash_lsh_index(&mut self, config: &MinHashLshConfig) -> Result<()> {
        // Get relation handle
        let mut rel_handle = self.get_relation(&config.base_relation, true)?;
        Self::reject_txtime_relation(&rel_handle, "::lsh create")?;

        // Check if index already exists
        if rel_handle.has_index(&config.index_name) {
            bail!(IndexAlreadyExists(
                config.index_name.to_string(),
                config.index_name.to_string()
            ));
        }

        let inv_idx_keys = rel_handle.metadata.keys.clone();
        let inv_idx_vals = vec![ColumnDef {
            name: SmartString::from("minhash"),
            typing: NullableColType {
                coltype: ColType::Bytes,
                nullable: false,
            },
            default_gen: None,
        }];

        let mut idx_keys = vec![ColumnDef {
            name: SmartString::from("hash"),
            typing: NullableColType {
                coltype: ColType::Bytes,
                nullable: false,
            },
            default_gen: None,
        }];
        for k in rel_handle.metadata.keys.iter() {
            idx_keys.push(ColumnDef {
                name: format!("src_{}", k.name).into(),
                typing: k.typing.clone(),
                default_gen: None,
            });
        }
        let idx_vals = vec![];

        let idx_handle = self.write_idx_relation(
            &config.base_relation,
            &config.index_name,
            idx_keys,
            idx_vals,
        )?;

        let inv_idx_handle = self.write_idx_relation(
            &config.base_relation,
            &format!("{}:inv", config.index_name),
            inv_idx_keys,
            inv_idx_vals,
        )?;

        // add index to relation
        let params = LshParams::find_optimal_params(
            config.target_threshold.0,
            config.n_perm,
            &Weights(
                config.false_positive_weight.0,
                config.false_negative_weight.0,
            ),
        );
        let num_perm = params.b * params.r;
        let perms = HashPermutations::new(num_perm);
        let manifest = MinHashLshIndexManifest {
            base_relation: config.base_relation.clone(),
            index_name: config.index_name.clone(),
            extractor: config.extractor.clone(),
            n_gram: config.n_gram,
            tokenizer: config.tokenizer.clone(),
            filters: config.filters.clone(),
            num_perm,
            n_bands: params.b,
            n_rows_in_band: params.r,
            threshold: config.target_threshold.0,
            perms: perms.as_bytes().to_vec(),
        };

        // populate index
        let tokenizer =
            self.tokenizers
                .get(&idx_handle.name, &manifest.tokenizer, &manifest.filters)?;
        let parsed = CozoScriptParser::parse(Rule::expr, &manifest.extractor)
            .into_diagnostic()?
            .next()
            .unwrap();
        let mut code_expr = build_expr(parsed, &Default::default())?;
        let binding_map = rel_handle.raw_binding_map();
        code_expr.fill_binding_indices(&binding_map)?;
        let extractor = code_expr.compile()?;

        let mut stack = vec![];

        let hash_perms = manifest.get_hash_perms();
        let mut existing = TempCollector::default();
        for tuple in rel_handle.scan_all(self) {
            existing.push(tuple?);
        }

        for tuple in existing.into_iter() {
            self.put_lsh_index_item(
                &tuple,
                &extractor,
                &mut stack,
                &tokenizer,
                &rel_handle,
                &idx_handle,
                &inv_idx_handle,
                &manifest,
                &hash_perms,
            )?;
        }

        rel_handle.lsh_indices.insert(
            manifest.index_name.clone(),
            (idx_handle, inv_idx_handle, manifest),
        );

        // update relation metadata
        let new_encoded =
            vec![DataValue::from(&rel_handle.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel_handle
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;

        Ok(())
    }

    pub(crate) fn create_fts_index(&mut self, config: &FtsIndexConfig) -> Result<()> {
        // Get relation handle
        let mut rel_handle = self.get_relation(&config.base_relation, true)?;
        Self::reject_txtime_relation(&rel_handle, "::fts create")?;

        // Check if index already exists
        if rel_handle.has_index(&config.index_name) {
            bail!(IndexAlreadyExists(
                config.index_name.to_string(),
                config.index_name.to_string()
            ));
        }

        // Build key columns definitions
        let mut idx_keys: Vec<ColumnDef> = vec![ColumnDef {
            name: SmartString::from("word"),
            typing: NullableColType {
                coltype: ColType::String,
                nullable: false,
            },
            default_gen: None,
        }];

        for k in rel_handle.metadata.keys.iter() {
            idx_keys.push(ColumnDef {
                name: format!("src_{}", k.name).into(),
                typing: k.typing.clone(),
                default_gen: None,
            });
        }

        let col_type = NullableColType {
            coltype: ColType::List {
                eltype: Box::new(NullableColType {
                    coltype: ColType::Int,
                    nullable: false,
                }),
                len: None,
            },
            nullable: false,
        };

        let non_idx_keys: Vec<ColumnDef> = vec![
            ColumnDef {
                name: SmartString::from("offset_from"),
                typing: col_type.clone(),
                default_gen: None,
            },
            ColumnDef {
                name: SmartString::from("offset_to"),
                typing: col_type.clone(),
                default_gen: None,
            },
            ColumnDef {
                name: SmartString::from("position"),
                typing: col_type,
                default_gen: None,
            },
            ColumnDef {
                name: SmartString::from("total_length"),
                typing: NullableColType {
                    coltype: ColType::Int,
                    nullable: false,
                },
                default_gen: None,
            },
        ];

        let idx_handle = self.write_idx_relation(
            &config.base_relation,
            &config.index_name,
            idx_keys,
            non_idx_keys,
        )?;

        // add index to relation
        let manifest = FtsIndexManifest {
            base_relation: config.base_relation.clone(),
            index_name: config.index_name.clone(),
            extractor: config.extractor.clone(),
            tokenizer: config.tokenizer.clone(),
            filters: config.filters.clone(),
        };

        // populate index
        let tokenizer =
            self.tokenizers
                .get(&idx_handle.name, &manifest.tokenizer, &manifest.filters)?;

        let parsed = CozoScriptParser::parse(Rule::expr, &manifest.extractor)
            .into_diagnostic()?
            .next()
            .unwrap();
        let mut code_expr = build_expr(parsed, &Default::default())?;
        let binding_map = rel_handle.raw_binding_map();
        code_expr.fill_binding_indices(&binding_map)?;
        let extractor = code_expr.compile()?;

        let mut existing = TempCollector::default();
        for tuple in rel_handle.scan_all(self) {
            existing.push(tuple?);
        }
        let tuples: Vec<Tuple> = existing.into_iter().collect();

        // Bulk-populate (mnestic fork). The index relation above is freshly
        // created and empty, so no del pass is needed (the old code tokenised
        // every document a second time to delete postings that could not
        // exist). Tokenisation + row encoding are pure, so they fan out across
        // worker threads; writes and doc-stats stay on this thread.
        let threads = crate::runtime::hnsw_build::build_threads()
            .max(1)
            .min(tuples.len().max(1));
        let mut total_tokens: u64 = 0;
        let mut n_docs: u64 = 0;
        if threads <= 1 || tuples.len() < 64 {
            let mut stack = vec![];
            for tuple in &tuples {
                let (rows, count) = encode_fts_rows_for_tuple(
                    tuple,
                    &extractor,
                    &mut stack,
                    &tokenizer,
                    &rel_handle,
                    &idx_handle,
                )?;
                if count > 0 {
                    total_tokens += count as u64;
                    n_docs += 1;
                }
                for (key_bytes, val_bytes) in rows {
                    self.store_tx.put(&key_bytes, &val_bytes)?;
                }
            }
        } else {
            let chunk_size = tuples.len().div_ceil(threads);
            type EncodedChunk = (Vec<(Vec<u8>, Vec<u8>)>, u64, u64);
            let chunk_results: Vec<Result<EncodedChunk>> = std::thread::scope(|s| {
                let handles: Vec<_> = tuples
                    .chunks(chunk_size)
                    .map(|chunk| {
                        let extractor = &extractor;
                        let tokenizer = &tokenizer;
                        let rel_handle = &rel_handle;
                        let idx_handle = &idx_handle;
                        s.spawn(move || {
                            let mut stack = vec![];
                            let mut rows = vec![];
                            let mut total_tokens: u64 = 0;
                            let mut n_docs: u64 = 0;
                            for tuple in chunk {
                                let (tuple_rows, count) = encode_fts_rows_for_tuple(
                                    tuple, extractor, &mut stack, tokenizer, rel_handle, idx_handle,
                                )?;
                                if count > 0 {
                                    total_tokens += count as u64;
                                    n_docs += 1;
                                }
                                rows.extend(tuple_rows);
                            }
                            Ok((rows, total_tokens, n_docs))
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("FTS index build worker panicked"))
                    .collect()
            });
            for result in chunk_results {
                let (rows, chunk_tokens, chunk_docs) = result?;
                total_tokens += chunk_tokens;
                n_docs += chunk_docs;
                for (key_bytes, val_bytes) in rows {
                    self.store_tx.put(&key_bytes, &val_bytes)?;
                }
            }
        }

        // Publish the authoritative corpus doc-stats counter for this freshly
        // built index (mnestic fork, Bet 1b) so `avgdl` is an O(1) read rather
        // than a per-query scan. Totals were counted exactly during the build,
        // so no index scan is needed.
        self.seed_fts_doc_stats(&idx_handle, total_tokens, n_docs)?;

        rel_handle
            .fts_indices
            .insert(manifest.index_name.clone(), (idx_handle, manifest));

        // update relation metadata
        let new_encoded =
            vec![DataValue::from(&rel_handle.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel_handle
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;

        Ok(())
    }

    /// Validate an HNSW index config, create its (empty) index relation, and
    /// build the manifest + compiled filter (mnestic fork). Shared by the
    /// in-transaction build (`create_hnsw_index`) and the non-blocking off-lock
    /// build orchestrated at the `Db` level. Does NOT scan/build/publish.
    pub(crate) fn prepare_hnsw_index(
        &mut self,
        config: &HnswIndexConfig,
    ) -> Result<(
        RelationHandle,
        RelationHandle,
        HnswIndexManifest,
        Vec<Bytecode>,
    )> {
        // Get relation handle
        let rel_handle = self.get_relation(&config.base_relation, true)?;
        Self::reject_txtime_relation(&rel_handle, "::hnsw create")?;

        // Check if index already exists
        if rel_handle.has_index(&config.index_name) {
            bail!(IndexAlreadyExists(
                config.index_name.to_string(),
                config.index_name.to_string()
            ));
        }

        // Check that what we are indexing are really vectors
        if config.vec_fields.is_empty() {
            bail!("Cannot create HNSW index without vector fields");
        }
        let mut vec_field_indices = vec![];
        for field in config.vec_fields.iter() {
            let mut found = false;
            for (i, col) in rel_handle
                .metadata
                .keys
                .iter()
                .chain(rel_handle.metadata.non_keys.iter())
                .enumerate()
            {
                if col.name == *field {
                    let mut col_type = col.typing.coltype.clone();
                    if let ColType::List { eltype, .. } = &col_type {
                        col_type = eltype.coltype.clone();
                    }

                    if let ColType::Vec { eltype, len } = col_type {
                        if eltype != config.dtype {
                            bail!("Cannot create HNSW index with field {} of type {:?} (expected {:?})", field, eltype, config.dtype);
                        }
                        if len != config.vec_dim {
                            bail!("Cannot create HNSW index with field {} of dimension {} (expected {})", field, len, config.vec_dim);
                        }
                    } else {
                        bail!("Cannot create HNSW index with non-vector field {}", field)
                    }

                    found = true;
                    vec_field_indices.push(i);
                    break;
                }
            }
            if !found {
                bail!("Cannot create HNSW index with non-existent field {}", field);
            }
        }

        // Build key columns definitions
        let mut idx_keys: Vec<ColumnDef> = vec![ColumnDef {
            // layer -1 stores the self-loops
            name: SmartString::from("layer"),
            typing: NullableColType {
                coltype: ColType::Int,
                nullable: false,
            },
            default_gen: None,
        }];
        // for self-loops, fr and to are identical
        for prefix in ["fr", "to"] {
            for col in rel_handle.metadata.keys.iter() {
                let mut col = col.clone();
                col.name = SmartString::from(format!("{}_{}", prefix, col.name));
                idx_keys.push(col);
            }
            idx_keys.push(ColumnDef {
                name: SmartString::from(format!("{}__field", prefix)),
                typing: NullableColType {
                    coltype: ColType::Int,
                    nullable: false,
                },
                default_gen: None,
            });
            idx_keys.push(ColumnDef {
                name: SmartString::from(format!("{}__sub_idx", prefix)),
                typing: NullableColType {
                    coltype: ColType::Int,
                    nullable: false,
                },
                default_gen: None,
            });
        }

        // Build non-key columns definitions
        let non_idx_keys = vec![
            // For self-loops, stores the number of neighbours
            ColumnDef {
                name: SmartString::from("dist"),
                typing: NullableColType {
                    coltype: ColType::Float,
                    nullable: false,
                },
                default_gen: None,
            },
            // For self-loops, stores a hash of the neighbours, for conflict detection
            ColumnDef {
                name: SmartString::from("hash"),
                typing: NullableColType {
                    coltype: ColType::Bytes,
                    nullable: true,
                },
                default_gen: None,
            },
            ColumnDef {
                name: SmartString::from("ignore_link"),
                typing: NullableColType {
                    coltype: ColType::Bool,
                    nullable: false,
                },
                default_gen: None,
            },
        ];
        // create index relation
        let idx_handle = self.write_idx_relation(
            &config.base_relation,
            &config.index_name,
            idx_keys,
            non_idx_keys,
        )?;

        // add index to relation
        let manifest = HnswIndexManifest {
            base_relation: config.base_relation.clone(),
            index_name: config.index_name.clone(),
            vec_dim: config.vec_dim,
            dtype: config.dtype,
            vec_fields: vec_field_indices,
            distance: config.distance,
            ef_construction: config.ef_construction,
            m_neighbours: config.m_neighbours,
            m_max: config.m_neighbours,
            m_max0: config.m_neighbours * 2,
            level_multiplier: 1. / (config.m_neighbours as f64).ln(),
            index_filter: config.index_filter.clone(),
            extend_candidates: config.extend_candidates,
            keep_pruned_connections: config.keep_pruned_connections,
        };

        let filter = if let Some(f_code) = &manifest.index_filter {
            let parsed = CozoScriptParser::parse(Rule::expr, f_code)
                .into_diagnostic()?
                .next()
                .unwrap();
            let mut code_expr = build_expr(parsed, &Default::default())?;
            let binding_map = rel_handle.raw_binding_map();
            code_expr.fill_binding_indices(&binding_map)?;
            code_expr.compile()?
        } else {
            vec![]
        };

        Ok((rel_handle, idx_handle, manifest, filter))
    }

    /// Build an HNSW index entirely within this transaction (mnestic fork): the
    /// graph is constructed in the in-RAM temp store, then the caller publishes
    /// the data (SST ingest, or the per-key flush). Used by non-RocksDB backends
    /// and the `skip_locking` import/restore path; RocksDB uses the non-blocking
    /// off-lock build orchestrated at the `Db` level.
    pub(crate) fn create_hnsw_index(&mut self, config: &HnswIndexConfig) -> Result<RelationId> {
        let (rel_handle, mut idx_handle, manifest, filter) = self.prepare_hnsw_index(config)?;

        // populate index
        let mut all_tuples = TempCollector::default();
        for tuple in rel_handle.scan_all(self) {
            all_tuples.push(tuple?);
        }
        let filter_ref = if filter.is_empty() {
            None
        } else {
            Some(&filter)
        };
        // Build the whole graph in the in-RAM temp store (mnestic fork): marking
        // the index handle `is_temp` routes every neighbour read/write to the
        // temp BTreeMap instead of the pessimistic transaction's RocksDB
        // `WriteBatchWithIndex` overlay, whose cost grows with the index and
        // made the build superlinear.
        idx_handle.is_temp = true;
        let tuples: Vec<_> = all_tuples.into_iter().collect();
        self.hnsw_build_index(&manifest, &rel_handle, &idx_handle, filter_ref, &tuples)?;
        idx_handle.is_temp = false;
        let idx_id = idx_handle.id;

        self.insert_hnsw_index_meta(rel_handle, idx_handle, manifest, &config.index_name)?;
        Ok(idx_id)
    }

    /// Publish a built HNSW index by registering it in the base relation's
    /// metadata, so reads and incremental maintenance pick it up (mnestic fork).
    /// Transactional: becomes visible at `commit`.
    pub(crate) fn insert_hnsw_index_meta(
        &mut self,
        mut rel_handle: RelationHandle,
        idx_handle: RelationHandle,
        manifest: HnswIndexManifest,
        index_name: &str,
    ) -> Result<()> {
        rel_handle
            .hnsw_indices
            .insert(SmartString::from(index_name), (idx_handle, manifest));
        let new_encoded =
            vec![DataValue::from(&rel_handle.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel_handle
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;
        Ok(())
    }

    /// Reconcile an off-lock-built HNSW index against base-relation mutations
    /// that committed during the unlocked build window (mnestic fork). `idx_table`
    /// must be the live (non-temp) handle; the bulk graph for `snapshot_tuples`
    /// has already been ingested. Diffs the current base against the snapshot and
    /// applies exactly the steady-state incremental maintenance for the delta:
    /// inserts for new rows, remove+insert for changed rows, removes for deleted
    /// rows. A no-op when nothing changed during the build.
    pub(crate) fn reconcile_hnsw_index(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        snapshot_tuples: &[Tuple],
    ) -> Result<()> {
        let nkeys = orig_table.metadata.keys.len();
        let mut snap: BTreeMap<Vec<u8>, &Tuple> = BTreeMap::new();
        for t in snapshot_tuples {
            let k = orig_table.encode_key_for_store(&t[0..nkeys], Default::default())?;
            snap.insert(k, t);
        }
        let current: Vec<Tuple> = orig_table.scan_all(self).collect::<Result<Vec<_>>>()?;
        let mut stack = vec![];
        for cur in &current {
            let k = orig_table.encode_key_for_store(&cur[0..nkeys], Default::default())?;
            match snap.remove(&k) {
                None => {
                    // new row inserted during the build
                    self.hnsw_put(manifest, orig_table, idx_table, filter, &mut stack, cur)?;
                }
                Some(old) => {
                    if old != cur {
                        // row changed in place during the build
                        self.hnsw_remove(orig_table, idx_table, old)?;
                        self.hnsw_put(manifest, orig_table, idx_table, filter, &mut stack, cur)?;
                    }
                }
            }
        }
        // rows present at snapshot but gone now: deleted during the build
        let removed: Vec<&Tuple> = snap.into_values().collect();
        for old in removed {
            self.hnsw_remove(orig_table, idx_table, old)?;
        }
        Ok(())
    }

    /// Bulk-copy a freshly-built index relation from the in-RAM temp store to
    /// the persistent store (mnestic fork). The entries arrive in key-sorted
    /// order (the temp store is a `BTreeMap`). Used by engines that do not
    /// support SST ingest; the RocksDB path publishes via `ingest_sorted`.
    pub(crate) fn flush_temp_index_to_store(&mut self, idx_id: RelationId) -> Result<()> {
        let lower = Tuple::default().encode_as_key(idx_id);
        let upper = Tuple::default().encode_as_key(idx_id.next());
        let entries: Vec<(Vec<u8>, Vec<u8>)> = self
            .temp_store_tx
            .range_scan(&lower, &upper)
            .collect::<Result<Vec<_>>>()?;
        for (k, v) in entries {
            self.store_tx.put(&k, &v)?;
        }
        Ok(())
    }

    fn write_idx_relation(
        &mut self,
        base_name: &str,
        idx_name: &str,
        idx_keys: Vec<ColumnDef>,
        non_idx_keys: Vec<ColumnDef>,
    ) -> Result<RelationHandle> {
        let key_bindings = idx_keys
            .iter()
            .map(|col| Symbol::new(col.name.clone(), Default::default()))
            .collect();
        let dep_bindings = non_idx_keys
            .iter()
            .map(|col| Symbol::new(col.name.clone(), Default::default()))
            .collect();
        let idx_handle = InputRelationHandle {
            name: Symbol::new(format!("{}:{}", base_name, idx_name), Default::default()),
            metadata: StoredRelationMetadata {
                keys: idx_keys,
                non_keys: non_idx_keys,
            },
            key_bindings,
            dep_bindings,
            span: Default::default(),
        };
        let idx_handle = self.create_relation(idx_handle)?;
        Ok(idx_handle)
    }

    pub(crate) fn create_index(
        &mut self,
        rel_name: &Symbol,
        idx_name: &Symbol,
        cols: &[Symbol],
    ) -> Result<()> {
        // Get relation handle
        let mut rel_handle = self.get_relation(rel_name, true)?;
        Self::reject_txtime_relation(&rel_handle, "::index create")?;

        // Check if index already exists
        if rel_handle.has_index(&idx_name.name) {
            bail!(IndexAlreadyExists(
                idx_name.name.to_string(),
                rel_name.name.to_string()
            ));
        }

        // Build column definitions
        let mut col_defs = vec![];
        'outer: for col in cols.iter() {
            for orig_col in rel_handle
                .metadata
                .keys
                .iter()
                .chain(rel_handle.metadata.non_keys.iter())
            {
                if orig_col.name == col.name {
                    col_defs.push(orig_col.clone());
                    continue 'outer;
                }
            }

            #[derive(Debug, Error, Diagnostic)]
            #[error("column {0} in index {1} for relation {2} not found")]
            #[diagnostic(code(tx::col_in_idx_not_found))]
            pub(crate) struct ColInIndexNotFound(String, String, String);

            bail!(ColInIndexNotFound(
                col.name.to_string(),
                idx_name.name.to_string(),
                rel_name.name.to_string()
            ));
        }

        'outer: for key in rel_handle.metadata.keys.iter() {
            for col in cols.iter() {
                if col.name == key.name {
                    continue 'outer;
                }
            }
            col_defs.push(key.clone());
        }

        let key_bindings = col_defs
            .iter()
            .map(|col| Symbol::new(col.name.clone(), Default::default()))
            .collect_vec();
        let idx_meta = StoredRelationMetadata {
            keys: col_defs,
            non_keys: vec![],
        };

        // create index relation
        let idx_handle = InputRelationHandle {
            name: Symbol::new(
                format!("{}:{}", rel_name.name, idx_name.name),
                Default::default(),
            ),
            metadata: idx_meta,
            key_bindings,
            dep_bindings: vec![],
            span: Default::default(),
        };

        let idx_handle = self.create_relation(idx_handle)?;

        // populate index
        let extraction_indices = idx_handle
            .metadata
            .keys
            .iter()
            .map(|col| {
                for (i, kc) in rel_handle.metadata.keys.iter().enumerate() {
                    if kc.name == col.name {
                        return i;
                    }
                }
                for (i, kc) in rel_handle.metadata.non_keys.iter().enumerate() {
                    if kc.name == col.name {
                        return i + rel_handle.metadata.keys.len();
                    }
                }
                unreachable!()
            })
            .collect_vec();

        // A corrupt/truncated stored tuple (e.g. from an interrupted write)
        // must degrade to "left out of this index" with a loud error — not
        // panic the whole index build. Index builds run inside `::index
        // create`, which applications may execute while (re)initializing a
        // database: a panic here turns one bad row into an unopenable
        // database (observed in production 2026-06-12: a 3-element tuple in
        // a relation whose index needed column 5 made every open attempt
        // panic until the tenant was blacklisted).
        let mut skipped_corrupt = 0usize;
        let mut check_tuple = |tuple: &Tuple| -> bool {
            match extraction_indices.iter().find(|idx| **idx >= tuple.len()) {
                None => true,
                Some(&bad) => {
                    skipped_corrupt += 1;
                    error!(
                        "skipping corrupt tuple in '{}' while building index '{}': \
                         tuple has {} values, index needs column {}",
                        rel_handle.name,
                        idx_handle.name,
                        tuple.len(),
                        bad + 1
                    );
                    false
                }
            }
        };
        if self.store_tx.supports_par_put() {
            for tuple in rel_handle.scan_all(self) {
                let tuple = tuple?;
                if !check_tuple(&tuple) {
                    continue;
                }
                let extracted = extraction_indices
                    .iter()
                    .map(|idx| tuple[*idx].clone())
                    .collect_vec();
                let key = idx_handle.encode_key_for_store(&extracted, Default::default())?;
                self.store_tx.par_put(&key, &[])?;
            }
        } else {
            let mut existing = TempCollector::default();
            for tuple in rel_handle.scan_all(self) {
                existing.push(tuple?);
            }
            for tuple in existing.into_iter() {
                if !check_tuple(&tuple) {
                    continue;
                }
                let extracted = extraction_indices
                    .iter()
                    .map(|idx| tuple[*idx].clone())
                    .collect_vec();
                let key = idx_handle.encode_key_for_store(&extracted, Default::default())?;
                self.store_tx.put(&key, &[])?;
            }
        }
        if skipped_corrupt > 0 {
            error!(
                "index '{}' built with {} corrupt tuple(s) skipped — the base \
                 relation '{}' needs repair",
                idx_handle.name, skipped_corrupt, rel_handle.name
            );
        }

        // add index to relation
        rel_handle
            .indices
            .insert(idx_name.name.clone(), (idx_handle, extraction_indices));

        // update relation metadata
        let new_encoded =
            vec![DataValue::from(&rel_name.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel_handle
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;

        Ok(())
    }

    pub(crate) fn remove_index(
        &mut self,
        rel_name: &Symbol,
        idx_name: &Symbol,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut rel = self.get_relation(rel_name, true)?;
        let is_lsh = rel.lsh_indices.contains_key(&idx_name.name);
        let is_fts = rel.fts_indices.contains_key(&idx_name.name);
        if is_lsh || is_fts {
            self.tokenizers.named_cache.write().unwrap().clear();
            self.tokenizers.hashed_cache.write().unwrap().clear();
        }
        if rel.indices.remove(&idx_name.name).is_none()
            && rel.hnsw_indices.remove(&idx_name.name).is_none()
            && rel.lsh_indices.remove(&idx_name.name).is_none()
            && rel.fts_indices.remove(&idx_name.name).is_none()
        {
            #[derive(Debug, Error, Diagnostic)]
            #[error("index {0} for relation {1} not found")]
            #[diagnostic(code(tx::idx_not_found))]
            pub(crate) struct IndexNotFound(String, String);

            bail!(IndexNotFound(idx_name.to_string(), rel_name.to_string()));
        }

        let mut to_clean =
            self.destroy_relation(&format!("{}:{}", rel_name.name, idx_name.name))?;
        if is_lsh {
            to_clean.extend(
                self.destroy_relation(&format!("{}:{}:inv", rel_name.name, idx_name.name))?,
            );
        }

        let new_encoded =
            vec![DataValue::from(&rel_name.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel.serialize(&mut Serializer::new(&mut meta_val).with_struct_map()).unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;

        Ok(to_clean)
    }

    pub(crate) fn rename_relation(&mut self, old: &Symbol, new: &Symbol) -> Result<()> {
        if old.name.starts_with('_') || new.name.starts_with('_') {
            bail!("Bad name given");
        }
        let new_key = DataValue::Str(new.name.clone());
        let new_encoded = vec![new_key].encode_as_key(RelationId::SYSTEM);

        if self.store_tx.exists(&new_encoded, true)? {
            bail!(RelNameConflictError(new.name.to_string()))
        };

        let old_key = DataValue::Str(old.name.clone());
        let old_encoded = vec![old_key].encode_as_key(RelationId::SYSTEM);

        let mut rel = self.get_relation(old, true)?;
        if rel.access_level < AccessLevel::Normal {
            bail!(InsufficientAccessLevel(
                rel.name.to_string(),
                "renaming relation".to_string(),
                rel.access_level
            ));
        }
        rel.name = new.name.clone();

        let mut meta_val = vec![];
        rel.serialize(&mut Serializer::new(&mut meta_val).with_struct_map()).unwrap();
        self.store_tx.del(&old_encoded)?;
        self.store_tx.put(&new_encoded, &meta_val)?;

        Ok(())
    }
    pub(crate) fn rename_temp_relation(&mut self, old: Symbol, new: Symbol) -> Result<()> {
        let new_key = DataValue::Str(new.name.clone());
        let new_encoded = vec![new_key].encode_as_key(RelationId::SYSTEM);

        if self.temp_store_tx.exists(&new_encoded, true)? {
            bail!(RelNameConflictError(new.name.to_string()))
        };

        let old_key = DataValue::Str(old.name.clone());
        let old_encoded = vec![old_key].encode_as_key(RelationId::SYSTEM);

        let mut rel = self.get_relation(&old, true)?;
        rel.name = new.name;

        let mut meta_val = vec![];
        rel.serialize(&mut Serializer::new(&mut meta_val).with_struct_map()).unwrap();
        self.temp_store_tx.del(&old_encoded)?;
        self.temp_store_tx.put(&new_encoded, &meta_val)?;

        Ok(())
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Insufficient access level {2} for {1} on stored relation '{0}'")]
#[diagnostic(code(tx::insufficient_access_level))]
pub(crate) struct InsufficientAccessLevel(
    pub(crate) String,
    pub(crate) String,
    pub(crate) AccessLevel,
);

#[cfg(test)]
mod catalog_compat_tests {
    use super::RelationHandle;
    use rmp_serde::Serializer;
    use serde::Serialize;

    /// Real, on-disk `edge` relation catalog captured from a production
    /// mindgraph graph created before mnestic 0.10.0. It is a **13-field
    /// positional msgpack array** (rmp_serde struct-as-array, written by an
    /// index-creation path predating `with_struct_map`) — i.e. it has NO
    /// `tt_gc_floor` element.
    ///
    /// Regression guard for the 0.10.0 outage: `tt_gc_floor` was added
    /// mid-struct, so positional decode misread the `indices` map as
    /// `Option<i64>` and every legacy graph failed to open with
    /// "Cannot deserialize relation metadata from bytes". Keeping the field
    /// last (with `#[serde(default)]`) makes the trailing element optional.
    const LEGACY_EDGE_CATALOG: &[u8] = &[
    157, 164, 101, 100, 103, 101, 2, 146, 145, 147, 163, 117, 105, 100, 146, 166,
    83, 116, 114, 105, 110, 103, 194, 192, 155, 147, 168, 102, 114, 111, 109, 95,
    117, 105, 100, 146, 166, 83, 116, 114, 105, 110, 103, 194, 192, 147, 166, 116,
    111, 95, 117, 105, 100, 146, 166, 83, 116, 114, 105, 110, 103, 194, 192, 147,
    169, 101, 100, 103, 101, 95, 116, 121, 112, 101, 146, 166, 83, 116, 114, 105,
    110, 103, 194, 192, 147, 165, 108, 97, 121, 101, 114, 146, 166, 83, 116, 114,
    105, 110, 103, 194, 192, 147, 170, 99, 114, 101, 97, 116, 101, 100, 95, 97,
    116, 146, 165, 70, 108, 111, 97, 116, 194, 192, 147, 170, 117, 112, 100, 97,
    116, 101, 100, 95, 97, 116, 146, 165, 70, 108, 111, 97, 116, 194, 192, 147,
    167, 118, 101, 114, 115, 105, 111, 110, 146, 163, 73, 110, 116, 194, 129, 165,
    67, 111, 110, 115, 116, 145, 129, 163, 78, 117, 109, 129, 163, 73, 110, 116,
    1, 147, 170, 99, 111, 110, 102, 105, 100, 101, 110, 99, 101, 146, 165, 70,
    108, 111, 97, 116, 194, 129, 165, 67, 111, 110, 115, 116, 145, 129, 163, 78,
    117, 109, 129, 165, 70, 108, 111, 97, 116, 203, 63, 240, 0, 0, 0, 0,
    0, 0, 147, 166, 119, 101, 105, 103, 104, 116, 146, 165, 70, 108, 111, 97,
    116, 194, 129, 165, 67, 111, 110, 115, 116, 145, 129, 163, 78, 117, 109, 129,
    165, 70, 108, 111, 97, 116, 203, 63, 224, 0, 0, 0, 0, 0, 0, 147,
    172, 116, 111, 109, 98, 115, 116, 111, 110, 101, 95, 97, 116, 146, 165, 70,
    108, 111, 97, 116, 194, 129, 165, 67, 111, 110, 115, 116, 145, 129, 163, 78,
    117, 109, 129, 165, 70, 108, 111, 97, 116, 203, 0, 0, 0, 0, 0, 0,
    0, 0, 147, 165, 112, 114, 111, 112, 115, 146, 164, 74, 115, 111, 110, 194,
    129, 165, 65, 112, 112, 108, 121, 146, 174, 79, 80, 95, 74, 83, 79, 78,
    95, 79, 66, 74, 69, 67, 84, 144, 144, 144, 144, 166, 78, 111, 114, 109,
    97, 108, 194, 133, 168, 102, 114, 111, 109, 95, 105, 100, 120, 146, 157, 173,
    101, 100, 103, 101, 58, 102, 114, 111, 109, 95, 105, 100, 120, 8, 146, 147,
    147, 168, 102, 114, 111, 109, 95, 117, 105, 100, 146, 166, 83, 116, 114, 105,
    110, 103, 194, 192, 147, 169, 101, 100, 103, 101, 95, 116, 121, 112, 101, 146,
    166, 83, 116, 114, 105, 110, 103, 194, 192, 147, 163, 117, 105, 100, 146, 166,
    83, 116, 114, 105, 110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114,
    109, 97, 108, 194, 128, 128, 128, 128, 160, 147, 1, 3, 0, 171, 102, 114,
    111, 109, 95, 116, 111, 95, 105, 100, 120, 146, 157, 176, 101, 100, 103, 101,
    58, 102, 114, 111, 109, 95, 116, 111, 95, 105, 100, 120, 25, 146, 147, 147,
    168, 102, 114, 111, 109, 95, 117, 105, 100, 146, 166, 83, 116, 114, 105, 110,
    103, 194, 192, 147, 166, 116, 111, 95, 117, 105, 100, 146, 166, 83, 116, 114,
    105, 110, 103, 194, 192, 147, 163, 117, 105, 100, 146, 166, 83, 116, 114, 105,
    110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114, 109, 97, 108, 194,
    128, 128, 128, 128, 160, 147, 1, 2, 0, 166, 116, 111, 95, 105, 100, 120,
    146, 157, 171, 101, 100, 103, 101, 58, 116, 111, 95, 105, 100, 120, 9, 146,
    147, 147, 166, 116, 111, 95, 117, 105, 100, 146, 166, 83, 116, 114, 105, 110,
    103, 194, 192, 147, 169, 101, 100, 103, 101, 95, 116, 121, 112, 101, 146, 166,
    83, 116, 114, 105, 110, 103, 194, 192, 147, 163, 117, 105, 100, 146, 166, 83,
    116, 114, 105, 110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114, 109,
    97, 108, 194, 128, 128, 128, 128, 160, 147, 2, 3, 0, 173, 116, 111, 109,
    98, 115, 116, 111, 110, 101, 95, 105, 100, 120, 146, 157, 178, 101, 100, 103,
    101, 58, 116, 111, 109, 98, 115, 116, 111, 110, 101, 95, 105, 100, 120, 23,
    146, 146, 147, 172, 116, 111, 109, 98, 115, 116, 111, 110, 101, 95, 97, 116,
    146, 165, 70, 108, 111, 97, 116, 194, 129, 165, 67, 111, 110, 115, 116, 145,
    129, 163, 78, 117, 109, 129, 165, 70, 108, 111, 97, 116, 203, 0, 0, 0,
    0, 0, 0, 0, 0, 147, 163, 117, 105, 100, 146, 166, 83, 116, 114, 105,
    110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114, 109, 97, 108, 194,
    128, 128, 128, 128, 160, 146, 10, 0, 168, 116, 121, 112, 101, 95, 105, 100,
    120, 146, 157, 173, 101, 100, 103, 101, 58, 116, 121, 112, 101, 95, 105, 100,
    120, 10, 146, 146, 147, 169, 101, 100, 103, 101, 95, 116, 121, 112, 101, 146,
    166, 83, 116, 114, 105, 110, 103, 194, 192, 147, 163, 117, 105, 100, 146, 166,
    83, 116, 114, 105, 110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114,
    109, 97, 108, 194, 128, 128, 128, 128, 160, 146, 3, 0, 128, 128, 128, 160,
    ];

    #[test]
    fn legacy_catalog_without_tt_gc_floor() {
        // The exact failure mode from prod: this MUST decode.
        let handle = RelationHandle::decode(LEGACY_EDGE_CATALOG)
            .expect("legacy 13-field catalog must still decode after the field move");
        assert_eq!(handle.name, "edge");
        assert!(
            handle.tt_gc_floor.is_none(),
            "missing trailing field must default to None"
        );
        // full nested decode really happened (secondary index survived)
        assert!(handle.indices.contains_key("from_idx"));

        // And the current write format (self-describing map via with_struct_map)
        // round-trips too, so both on-disk encodings are readable.
        let mut buf = Vec::new();
        handle
            .serialize(&mut Serializer::new(&mut buf).with_struct_map())
            .unwrap();
        let reread = RelationHandle::decode(&buf).expect("map-encoded catalog must decode");
        assert_eq!(reread.name, "edge");
        assert!(reread.indices.contains_key("from_idx"));
    }
}
