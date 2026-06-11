/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use crate::data::expr::{eval_bytecode, eval_bytecode_pred, Bytecode};
use crate::data::program::{FtsScoreKind, FtsSearch};
use crate::data::tuple::{decode_tuple_from_key, Tuple, ENCODED_KEY_MIN_LEN};
use crate::data::value::LARGEST_UTF_CHAR;
use crate::fts::ast::{FtsExpr, FtsLiteral, FtsNear};
use crate::fts::tokenizer::TextAnalyzer;
use crate::parse::fts::parse_fts_query;
use crate::runtime::relation::RelationHandle;
use crate::runtime::transact::SessionTx;
use crate::{DataValue, SourceSpan};
use itertools::Itertools;
use miette::{bail, miette, Diagnostic, Result};
use ordered_float::OrderedFloat;
use rustc_hash::{FxHashMap, FxHashSet};
use smartstring::{LazyCompact, SmartString};
use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Default)]
pub(crate) struct FtsCache {
    total_n_cache: FxHashMap<SmartString<LazyCompact>, usize>,
}

impl FtsCache {
    fn get_n_for_relation(&mut self, rel: &RelationHandle, tx: &SessionTx<'_>) -> Result<usize> {
        Ok(match self.total_n_cache.entry(rel.name.clone()) {
            Entry::Vacant(v) => {
                let start = rel.encode_partial_key_for_store(&[]);
                let end = rel.encode_partial_key_for_store(&[DataValue::Bot]);
                let val = tx.store_tx.range_count(&start, &end)?;
                v.insert(val);
                val
            }
            Entry::Occupied(o) => *o.get(),
        })
    }
    /// Average document length (in tokens) over the indexed corpus — the BM25
    /// length-normalization denominator (mnestic fork, DEVELOPMENT.md Bet 1b).
    ///
    /// **O(1)** per query: reads the process-level doc-stats cache on the `Db`,
    /// seeding it with one deduplicated full scan of the index the first time it
    /// is touched in this process. Writes maintain the cache incrementally (see
    /// `SessionTx::bump_fts_doc_stats`), so the scan is paid once per process per
    /// index, never per query — and, deliberately, nothing here writes a shared
    /// storage key (a durable counter written from every document transaction
    /// makes all concurrent writers conflict on one RocksDB lock; that was the
    /// 0.8.3 design, reverted).
    fn get_avgdl_for_index(&mut self, idx: &RelationHandle, tx: &SessionTx<'_>) -> Result<f64> {
        let avgdl = |total: u64, n: u64| if n > 0 { total as f64 / n as f64 } else { 0.0 };
        let mut cache = tx.fts_doc_stats_cache.lock().unwrap();
        if let Some((total, n)) = cache.get(&idx.name).copied() {
            return Ok(avgdl(total, n));
        }
        let (total, n) = tx.scan_fts_doc_stats(idx)?;
        cache.insert(idx.name.clone(), (total, n));
        Ok(avgdl(total, n))
    }
}

struct PositionInfo {
    // from: u32,
    // to: u32,
    position: u32,
}

struct LiteralStats {
    key: Tuple,
    position_info: Vec<PositionInfo>,
    /// Total token count of the document this posting belongs to (stored per posting
    /// at index time as `vals[3]`); used for BM25 length normalization.
    doc_len: u32,
}

impl<'a> SessionTx<'a> {
    /// Reserved key under which the 0.8.3 design stored a durable corpus
    /// doc-stats counter. `DataValue::Bot` is the top key sentinel, so it sits
    /// *above* every `[term, …doc_key]` posting — never returned by a term range
    /// scan nor by the full-index doc scan (whose exclusive upper bound is
    /// exactly this key). Kept only so rebuilds can delete legacy counters;
    /// nothing reads or writes it anymore (every document transaction writing
    /// one shared key made all concurrent writers conflict on a single RocksDB
    /// lock, and the unlocked read-modify-write also lost updates).
    fn fts_stats_key(idx: &RelationHandle) -> Vec<u8> {
        idx.encode_partial_key_for_store(&[DataValue::Bot])
    }

    /// Apply a delta to the process-level doc-stats cache for `idx`, seeding it
    /// with a one-time scan of the existing postings if absent.
    ///
    /// Call BEFORE mutating the document's postings: the seed scan must observe
    /// the pre-mutation corpus so the delta is applied exactly once. The cache
    /// mutex is a leaf lock; holding it across the (one-time) seed scan keeps
    /// concurrent seeders from losing each other's deltas. Deltas from
    /// transactions that later roll back are not undone — `avgdl` is a smoothing
    /// denominator, the drift is negligible and clears on process restart or
    /// index rebuild.
    fn bump_fts_doc_stats(
        &self,
        idx: &RelationHandle,
        token_delta: i64,
        doc_delta: i64,
    ) -> Result<()> {
        let cache = self.fts_doc_stats_cache.clone();
        let mut guard = cache.lock().unwrap();
        let (total, n) = match guard.get(&idx.name).copied() {
            Some(stats) => stats,
            None => self.scan_fts_doc_stats(idx)?,
        };
        let total = (total as i64).saturating_add(token_delta).max(0) as u64;
        let n = (n as i64).saturating_add(doc_delta).max(0) as u64;
        guard.insert(idx.name.clone(), (total, n));
        Ok(())
    }

    /// Deduplicated full scan of the FTS index → `(total_tokens, n_docs)` over
    /// the documents that have at least one posting. Each document's length is
    /// stored redundantly on every posting (`vals[3]`), so we count each document
    /// key once. This is the legacy/seed path; the steady state reads the counter.
    pub(crate) fn scan_fts_doc_stats(&self, idx: &RelationHandle) -> Result<(u64, u64)> {
        let start = idx.encode_partial_key_for_store(&[]);
        let end = idx.encode_partial_key_for_store(&[DataValue::Bot]);
        let mut seen: FxHashSet<Tuple> = FxHashSet::default();
        let mut total: u64 = 0;
        for item in self.store_tx.range_scan(&start, &end) {
            let (kvec, vvec) = item?;
            let key_tuple = decode_tuple_from_key(&kvec, idx.metadata.keys.len());
            if seen.insert(key_tuple[1..].to_vec()) {
                let vals: Vec<DataValue> = rmp_serde::from_slice(&vvec[ENCODED_KEY_MIN_LEN..])
                    .map_err(|e| miette!("corrupt FTS posting value: {e}"))?;
                total += vals[3].get_int().unwrap_or(0).max(0) as u64;
            }
        }
        Ok((total, seen.len() as u64))
    }

    /// Recompute the doc-stats cache entry from a full scan. Called at the end
    /// of an index (re)build to publish authoritative corpus stats (and to clear
    /// drift from any rolled-back deltas). Also deletes the legacy durable
    /// counter a 0.8.3 build may have left behind.
    pub(crate) fn rebuild_fts_doc_stats(&mut self, idx: &RelationHandle) -> Result<()> {
        let stats = self.scan_fts_doc_stats(idx)?;
        self.seed_fts_doc_stats(idx, stats.0, stats.1)
    }

    /// Publish corpus stats already known exactly (e.g. counted during a bulk
    /// build), skipping the full index scan `rebuild_fts_doc_stats` pays. Also
    /// deletes the legacy durable counter a 0.8.3 build may have left behind.
    pub(crate) fn seed_fts_doc_stats(
        &mut self,
        idx: &RelationHandle,
        total_tokens: u64,
        n_docs: u64,
    ) -> Result<()> {
        self.fts_doc_stats_cache
            .lock()
            .unwrap()
            .insert(idx.name.clone(), (total_tokens, n_docs));
        let legacy_key = Self::fts_stats_key(idx);
        if self.store_tx.exists(&legacy_key, false)? {
            self.store_tx.del(&legacy_key)?;
        }
        Ok(())
    }

    fn fts_search_literal(
        &self,
        literal: &FtsLiteral,
        idx_handle: &RelationHandle,
    ) -> Result<Vec<LiteralStats>> {
        let start_key_str = &literal.value as &str;
        let start_key = vec![DataValue::Str(SmartString::from(start_key_str))];
        let mut end_key_str = literal.value.clone();
        end_key_str.push(LARGEST_UTF_CHAR);
        let end_key = vec![DataValue::Str(end_key_str)];
        let start_key_bytes = idx_handle.encode_partial_key_for_store(&start_key);
        let end_key_bytes = idx_handle.encode_partial_key_for_store(&end_key);
        let mut results = vec![];
        for item in self.store_tx.range_scan(&start_key_bytes, &end_key_bytes) {
            let (kvec, vvec) = item?;
            let key_tuple = decode_tuple_from_key(&kvec, idx_handle.metadata.keys.len());
            let found_str_key = key_tuple[0].get_str().unwrap();
            if literal.is_prefix {
                if !found_str_key.starts_with(start_key_str) {
                    break;
                }
            } else if found_str_key != start_key_str {
                break;
            }

            let vals: Vec<DataValue> = rmp_serde::from_slice(&vvec[ENCODED_KEY_MIN_LEN..]).unwrap();
            let froms = vals[0].get_slice().unwrap();
            let tos = vals[1].get_slice().unwrap();
            let positions = vals[2].get_slice().unwrap();
            let total_length = vals[3].get_int().unwrap();
            let position_info = froms
                .iter()
                .zip(tos.iter())
                .zip(positions.iter())
                .map(|(_, p)| PositionInfo {
                    // from: f.get_int().unwrap() as u32,
                    // to: t.get_int().unwrap() as u32,
                    position: p.get_int().unwrap() as u32,
                })
                .collect_vec();
            results.push(LiteralStats {
                key: key_tuple[1..].to_vec(),
                position_info,
                doc_len: total_length as u32,
            });
        }
        Ok(results)
    }
    fn fts_search_impl(
        &self,
        ast: &FtsExpr,
        config: &FtsSearch,
        n: usize,
        avgdl: f64,
    ) -> Result<FxHashMap<Tuple, f64>> {
        Ok(match ast {
            FtsExpr::Literal(l) => {
                let mut res = FxHashMap::default();
                let found_docs = self.fts_search_literal(l, &config.idx_handle)?;
                let found_docs_len = found_docs.len();
                for el in found_docs {
                    let score = Self::fts_compute_score(
                        el.position_info.len(),
                        found_docs_len,
                        n,
                        el.doc_len,
                        avgdl,
                        l.booster.0,
                        config,
                    );
                    res.insert(el.key, score);
                }
                res
            }
            FtsExpr::And(ls) => {
                let mut l_iter = ls.iter();
                let mut res = self.fts_search_impl(
                    l_iter.next().unwrap(),
                    config,
                    n,
                    avgdl,
                )?;
                for nxt in l_iter {
                    let nxt_res = self.fts_search_impl(nxt, config, n, avgdl)?;
                    res = res
                        .into_iter()
                        .filter_map(|(k, v)| nxt_res.get(&k).map(|nxt_v| (k, v + nxt_v)))
                        .collect();
                }
                res
            }
            FtsExpr::Or(ls) => {
                // BM25 sums each query term's contribution (a doc matching more terms
                // ranks higher); tf/tf_idf keep upstream's max-combine for compatibility.
                let sum_terms = config.score_kind == FtsScoreKind::Bm25;
                let mut res: FxHashMap<Tuple, f64> = FxHashMap::default();
                for nxt in ls {
                    let nxt_res = self.fts_search_impl(nxt, config, n, avgdl)?;
                    for (k, v) in nxt_res {
                        if let Some(old_v) = res.get_mut(&k) {
                            *old_v = if sum_terms { *old_v + v } else { (*old_v).max(v) };
                        } else {
                            res.insert(k, v);
                        }
                    }
                }
                res
            }
            FtsExpr::Near(FtsNear { literals, distance }) => {
                let mut l_it = literals.iter();
                let mut coll: FxHashMap<_, _> = FxHashMap::default();
                // The document length is identical across a doc's postings, so capture
                // it from the first literal's scan for BM25 length normalization.
                let mut doc_lens: FxHashMap<Tuple, u32> = FxHashMap::default();
                for first_el in self.fts_search_literal(l_it.next().unwrap(), &config.idx_handle)? {
                    doc_lens.insert(first_el.key.clone(), first_el.doc_len);
                    coll.insert(
                        first_el.key,
                        first_el
                            .position_info
                            .into_iter()
                            .map(|el| el.position)
                            .collect_vec(),
                    );
                }
                for lit_nxt in literals {
                    let el_res = self.fts_search_literal(lit_nxt, &config.idx_handle)?;
                    coll = el_res
                        .into_iter()
                        .filter_map(|x| match coll.remove(&x.key) {
                            None => None,
                            Some(prev_pos) => {
                                let mut inner_coll = FxHashSet::default();
                                for p in prev_pos {
                                    for pi in x.position_info.iter() {
                                        let cur = pi.position;
                                        if cur > p {
                                            if cur - p <= *distance {
                                                inner_coll.insert(p);
                                            }
                                        } else if p - cur <= *distance {
                                            inner_coll.insert(cur);
                                        }
                                    }
                                }
                                if inner_coll.is_empty() {
                                    None
                                } else {
                                    Some((x.key, inner_coll.into_iter().collect_vec()))
                                }
                            }
                        })
                        .collect();
                }
                let mut booster = 0.0;
                for lit in literals {
                    booster += lit.booster.0;
                }
                let coll_len = coll.len();
                coll.into_iter()
                    .map(|(k, cands)| {
                        let doc_len = doc_lens.get(&k).copied().unwrap_or(0);
                        let score = Self::fts_compute_score(
                            cands.len(),
                            coll_len,
                            n,
                            doc_len,
                            avgdl,
                            booster,
                            config,
                        );
                        (k, score)
                    })
                    .collect()
            }
            FtsExpr::Not(fst, snd) => {
                let mut res = self.fts_search_impl(fst, config, n, avgdl)?;
                for el in self
                    .fts_search_impl(snd, config, n, avgdl)?
                    .keys()
                {
                    res.remove(el);
                }
                res
            }
        })
    }
    fn fts_compute_score(
        tf: usize,
        n_found_docs: usize,
        n_total: usize,
        doc_len: u32,
        avgdl: f64,
        booster: f64,
        config: &FtsSearch,
    ) -> f64 {
        let tf = tf as f64;
        match config.score_kind {
            FtsScoreKind::Tf => tf * booster,
            FtsScoreKind::TfIdf => {
                let n_found_docs = n_found_docs as f64;
                let idf = (1.0 + (n_total as f64 - n_found_docs + 0.5) / (n_found_docs + 0.5)).ln();
                tf * idf * booster
            }
            FtsScoreKind::Bm25 => {
                // Okapi BM25: idf · tf·(k1+1) / (tf + k1·(1 − b + b·|D|/avgdl)) · booster
                let df = n_found_docs as f64;
                let idf = (1.0 + (n_total as f64 - df + 0.5) / (df + 0.5)).ln();
                let avgdl = if avgdl > 0.0 { avgdl } else { 1.0 };
                let norm = 1.0 - config.b + config.b * (doc_len as f64) / avgdl;
                let denom = tf + config.k1 * norm;
                let saturated = if denom > 0.0 {
                    tf * (config.k1 + 1.0) / denom
                } else {
                    0.0
                };
                idf * saturated * booster
            }
        }
    }
    pub(crate) fn fts_search(
        &self,
        q: &str,
        config: &FtsSearch,
        filter_code: &Option<(Vec<Bytecode>, SourceSpan)>,
        tokenizer: &TextAnalyzer,
        stack: &mut Vec<DataValue>,
        cache: &mut FtsCache,
    ) -> Result<Vec<Tuple>> {
        let ast = parse_fts_query(q)?.tokenize(tokenizer);
        if ast.is_empty() {
            return Ok(vec![]);
        }
        let n = match config.score_kind {
            FtsScoreKind::TfIdf | FtsScoreKind::Bm25 => {
                cache.get_n_for_relation(&config.base_handle, self)?
            }
            FtsScoreKind::Tf => 0,
        };
        let avgdl = if config.score_kind == FtsScoreKind::Bm25 {
            cache.get_avgdl_for_index(&config.idx_handle, self)?
        } else {
            0.0
        };
        let mut result: Vec<_> = self
            .fts_search_impl(&ast, config, n, avgdl)?
            .into_iter()
            .collect();
        result.sort_by_key(|(_, score)| Reverse(OrderedFloat(*score)));
        if config.filter.is_none() {
            result.truncate(config.k);
        }

        let mut ret = Vec::with_capacity(config.k);
        for (found_key, score) in result {
            let mut cand_tuple = config
                .base_handle
                .get(self, &found_key)?
                .ok_or_else(|| miette!("corrupted index"))?;

            if config.bind_score.is_some() {
                cand_tuple.push(DataValue::from(score));
            }

            if let Some((code, span)) = filter_code {
                if !eval_bytecode_pred(code, &cand_tuple, stack, *span)? {
                    continue;
                }
            }

            ret.push(cand_tuple);
            if ret.len() >= config.k {
                break;
            }
        }
        Ok(ret)
    }
    pub(crate) fn put_fts_index_item(
        &mut self,
        tuple: &[DataValue],
        extractor: &[Bytecode],
        stack: &mut Vec<DataValue>,
        tokenizer: &TextAnalyzer,
        rel_handle: &RelationHandle,
        idx_handle: &RelationHandle,
    ) -> Result<()> {
        let (rows, count) =
            encode_fts_rows_for_tuple(tuple, extractor, stack, tokenizer, rel_handle, idx_handle)?;
        // Maintain the process-level doc-stats cache (mnestic fork, Bet 1b) so
        // `avgdl` is an O(1) read. Done *before* writing this document's postings
        // so a seed-on-absent scan sees the pre-insert corpus and we add the new
        // document exactly once. `count == 0` (no tokens ⇒ no postings) is skipped,
        // matching the scan, which only counts documents that have postings.
        // (The normal update path is del-then-put, so the old document is already
        // subtracted; an FTS-only relation with no secondary index does not call
        // `del` on update and can drift, mirroring upstream's posting leak there.)
        if count > 0 {
            self.bump_fts_doc_stats(idx_handle, count, 1)?;
        }
        for (key_bytes, val_bytes) in rows {
            self.store_tx.put(&key_bytes, &val_bytes)?;
        }
        Ok(())
    }
    pub(crate) fn del_fts_index_item(
        &mut self,
        tuple: &[DataValue],
        extractor: &[Bytecode],
        stack: &mut Vec<DataValue>,
        tokenizer: &TextAnalyzer,
        rel_handle: &RelationHandle,
        idx_handle: &RelationHandle,
    ) -> Result<()> {
        let to_index = match eval_bytecode(extractor, tuple, stack)? {
            DataValue::Null => return Ok(()),
            DataValue::Str(s) => s,
            val => {
                #[derive(Debug, Diagnostic, Error)]
                #[error("FTS index extractor must return a string, got {0}")]
                #[diagnostic(code(eval::fts::extractor::invalid_return_type))]
                struct FtsExtractError(String);

                bail!(FtsExtractError(format!("{}", val)))
            }
        };
        let mut token_stream = tokenizer.token_stream(&to_index);
        let mut collector = FxHashSet::default();
        let mut count = 0i64;
        while let Some(token) = token_stream.next() {
            let text = SmartString::<LazyCompact>::from(&token.text);
            collector.insert(text);
            count += 1;
        }
        let mut key = Vec::with_capacity(1 + rel_handle.metadata.keys.len());
        key.push(DataValue::Bot);
        for k in &tuple[..rel_handle.metadata.keys.len()] {
            key.push(k.clone());
        }
        // Maintain the process-level doc-stats cache (mnestic fork, Bet 1b) — but
        // only if this document is actually indexed (probe one of its postings).
        // That guards against a delete of an unindexed row and the del-then-put
        // refresh in `create_fts_index`, where `del` runs over a not-yet-indexed
        // row. Done before the postings are removed so a seed-on-absent scan
        // still sees them.
        if count > 0 {
            if let Some(term) = collector.iter().next() {
                let mut probe = key.clone();
                probe[0] = DataValue::Str(term.clone());
                let probe_bytes = idx_handle.encode_key_for_store(&probe, Default::default())?;
                if self.store_tx.exists(&probe_bytes, false)? {
                    self.bump_fts_doc_stats(idx_handle, -count, -1)?;
                }
            }
        }
        for text in collector {
            key[0] = DataValue::Str(text);
            let key_bytes = idx_handle.encode_key_for_store(&key, Default::default())?;
            self.store_tx.del(&key_bytes)?;
        }
        Ok(())
    }
}

/// Tokenise one document and encode its posting rows — the pure half of
/// `put_fts_index_item` (mnestic fork). Needs no transaction access, so bulk
/// index builds can run it on worker threads; the caller writes the returned
/// rows and applies the token `count` to the doc-stats cache.
pub(crate) fn encode_fts_rows_for_tuple(
    tuple: &[DataValue],
    extractor: &[Bytecode],
    stack: &mut Vec<DataValue>,
    tokenizer: &TextAnalyzer,
    rel_handle: &RelationHandle,
    idx_handle: &RelationHandle,
) -> Result<(Vec<(Vec<u8>, Vec<u8>)>, i64)> {
    let to_index = match eval_bytecode(extractor, tuple, stack)? {
        DataValue::Null => return Ok((vec![], 0)),
        DataValue::Str(s) => s,
        val => {
            #[derive(Debug, Diagnostic, Error)]
            #[error("FTS index extractor must return a string, got {0}")]
            #[diagnostic(code(eval::fts::extractor::invalid_return_type))]
            struct FtsExtractError(String);

            bail!(FtsExtractError(format!("{}", val)))
        }
    };
    let mut token_stream = tokenizer.token_stream(&to_index);
    let mut collector: HashMap<_, (Vec<_>, Vec<_>, Vec<_>), _> = FxHashMap::default();
    let mut count = 0i64;
    while let Some(token) = token_stream.next() {
        let text = SmartString::<LazyCompact>::from(&token.text);
        let (fr, to, position) = collector.entry(text).or_default();
        fr.push(DataValue::from(token.offset_from as i64));
        to.push(DataValue::from(token.offset_to as i64));
        position.push(DataValue::from(token.position as i64));
        count += 1;
    }
    let mut key = Vec::with_capacity(1 + rel_handle.metadata.keys.len());
    key.push(DataValue::Bot);
    for k in &tuple[..rel_handle.metadata.keys.len()] {
        key.push(k.clone());
    }
    let mut val = vec![
        DataValue::Bot,
        DataValue::Bot,
        DataValue::Bot,
        DataValue::from(count),
    ];
    let mut rows = Vec::with_capacity(collector.len());
    for (text, (from, to, position)) in collector {
        key[0] = DataValue::Str(text);
        val[0] = DataValue::List(from);
        val[1] = DataValue::List(to);
        val[2] = DataValue::List(position);
        let key_bytes = idx_handle.encode_key_for_store(&key, Default::default())?;
        let val_bytes = idx_handle.encode_val_only_for_store(&val, Default::default())?;
        rows.push((key_bytes, val_bytes));
    }
    Ok((rows, count))
}
