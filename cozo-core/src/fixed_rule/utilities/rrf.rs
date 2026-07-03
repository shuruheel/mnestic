/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Reciprocal Rank Fusion (RRF) — a mnestic fork addition for hybrid retrieval.
//!
//! Combines several ranked result lists (e.g. vector/HNSW, full-text/FTS, graph
//! traversal) into one fused ranking. The classic RRF score for an item is
//! `Σ_lists 1 / (k + rank_in_list)`, where `rank_in_list` is the item's 1-based
//! position in that list and `k` is a smoothing constant (default 60).
//!
//! Input: a single relation `[list_id, item, score]`. Rows are grouped by
//! `list_id`; within each list, items are ranked by `score` (descending — higher
//! is better — by default) and the reciprocal-rank contributions are summed per
//! item across lists. Output: `[item, fused_score]` (the caller sorts/limits, or
//! pipes it into further Datalog).
//!
//! With `detailed: true` the output is instead the long-format
//! `[item, fused_score, list_id, leg_rank, leg_score]` — one row per
//! *(item, contributing list)*, where `leg_rank` is the 1-based within-list rank
//! the fusion actually used and `leg_score` is the (deduplicated best) raw score
//! in that list. `fused_score` repeats on every row for an item. This exposes
//! exactly *why* an item ranked where it did; lists an item did not appear in
//! contribute no row.
//!
//! Why a fixed rule and not plain Datalog: Cozo can already *sum* reciprocal
//! contributions, but it has no way to assign a *rank position within a group*.
//! That intra-list ranking is exactly what this rule provides. Mixed-direction
//! lists (HNSW distance is ascending-good, FTS relevance is descending-good)
//! should be normalised by the caller so that higher = better, or split into
//! separate invocations.
//!
//! Example:
//! ```cozo
//! sem[item, score] := ~nodes:embedding{ item | query: $q, k: 50 }, score = ...
//! txt[item, score] := ~nodes:text{ item | query: $kw, k: 50 }, score = ...
//! combined[lid, item, score] := sem[item, score], lid = "semantic"
//! combined[lid, item, score] := txt[item, score], lid = "text"
//! ?[item, fused] <~ ReciprocalRankFusion(combined[lid, item, score], k: 60)
//! ```

use std::collections::BTreeMap;

use miette::{bail, Result};
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::{CannotDetermineArity, FixedRule, FixedRulePayload};
use crate::parse::SourceSpan;
use crate::runtime::db::Poison;
use crate::runtime::temp_store::RegularTempStore;

pub(crate) struct ReciprocalRankFusion;

impl FixedRule for ReciprocalRankFusion {
    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &mut RegularTempStore,
        poison: Poison,
    ) -> Result<()> {
        let in_rel = payload.get_input(0)?;
        // `k` is clamped to >= 0 so that `k + rank` (rank >= 1) can never divide
        // by zero or go negative.
        let k = payload.float_option("k", Some(60.0))?.max(0.0);
        let descending = payload.bool_option("descending", Some(true))?;
        let detailed = payload.bool_option("detailed", Some(false))?;

        // Group (item, score) by list_id.
        let mut lists: BTreeMap<DataValue, Vec<(DataValue, DataValue)>> = BTreeMap::new();
        for tuple in in_rel.iter()? {
            let tuple = tuple?;
            if tuple.len() != 3 {
                bail!(
                    "ReciprocalRankFusion expects a 3-column input [list_id, item, score], \
                     got a row of arity {}",
                    tuple.len()
                );
            }
            let mut it = tuple.into_iter();
            let list_id = it.next().unwrap();
            let item = it.next().unwrap();
            let score = it.next().unwrap();
            // Reject non-finite scores: a NaN sorts above +inf under the DataValue
            // total order, so it would silently grab rank 1 and poison the fusion.
            if let Some(f) = score.get_float() {
                if !f.is_finite() {
                    bail!("ReciprocalRankFusion: score (column 3) must be finite, got {f}");
                }
            }
            lists.entry(list_id).or_default().push((item, score));
            poison.check()?;
        }

        // Fuse: rank within each list, accumulate 1 / (k + rank) per item.
        let mut fused: BTreeMap<DataValue, f64> = BTreeMap::new();
        // item -> (list_id, leg_rank, leg_score) contributions, kept only when
        // `detailed` so the plain path stays allocation-free.
        let mut contribs: BTreeMap<DataValue, Vec<(DataValue, f64, DataValue)>> = BTreeMap::new();
        for (list_id, entries) in lists {
            // An item may appear more than once in a list; keep its best score.
            let mut best: BTreeMap<DataValue, DataValue> = BTreeMap::new();
            for (item, score) in entries {
                match best.get_mut(&item) {
                    Some(cur) => {
                        let better = if descending {
                            score > *cur
                        } else {
                            score < *cur
                        };
                        if better {
                            *cur = score;
                        }
                    }
                    None => {
                        best.insert(item, score);
                    }
                }
            }
            // Rank by score. Ties get consecutive positions (deterministic via the
            // item ordering from the BTreeMap), which is acceptable for RRF.
            let mut ranked: Vec<(DataValue, DataValue)> = best.into_iter().collect();
            if descending {
                ranked.sort_by(|a, b| b.1.cmp(&a.1));
            } else {
                ranked.sort_by(|a, b| a.1.cmp(&b.1));
            }
            for (idx, (item, score)) in ranked.into_iter().enumerate() {
                let rank = (idx + 1) as f64;
                *fused.entry(item.clone()).or_insert(0.0) += 1.0 / (k + rank);
                if detailed {
                    contribs
                        .entry(item)
                        .or_default()
                        .push((list_id.clone(), rank, score));
                }
            }
            poison.check()?;
        }

        if detailed {
            for (item, fused_score) in fused {
                let item_contribs = contribs.remove(&item).unwrap_or_default();
                for (list_id, leg_rank, leg_score) in item_contribs {
                    out.put(vec![
                        item.clone(),
                        DataValue::from(fused_score),
                        list_id,
                        DataValue::from(leg_rank),
                        leg_score,
                    ]);
                }
            }
        } else {
            for (item, score) in fused {
                out.put(vec![item, DataValue::from(score)]);
            }
        }
        Ok(())
    }

    fn arity(
        &self,
        options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        span: SourceSpan,
    ) -> Result<usize> {
        // [item, fused_score], or with `detailed: true` the long-format
        // [item, fused_score, list_id, leg_rank, leg_score].
        match options.get("detailed") {
            None
            | Some(Expr::Const {
                val: DataValue::Bool(false),
                ..
            }) => Ok(2),
            Some(Expr::Const {
                val: DataValue::Bool(true),
                ..
            }) => Ok(5),
            _ => bail!(CannotDetermineArity(
                "ReciprocalRankFusion".to_string(),
                "invalid option 'detailed' given, expect a constant boolean".to_string(),
                span
            )),
        }
    }
}
