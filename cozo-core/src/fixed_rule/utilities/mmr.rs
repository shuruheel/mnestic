/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Maximal Marginal Relevance (MMR) — a mnestic fork addition for hybrid
//! retrieval. Re-ranks a candidate set to balance relevance against diversity,
//! so a result list isn't dominated by near-duplicates (a common agentic-memory
//! failure: recalling five paraphrases of the same fact).
//!
//! At each step MMR greedily selects the candidate maximising
//! `λ · relevance(i) − (1 − λ) · max_{j ∈ selected} cosine_sim(vec_i, vec_j)`.
//! `λ = 1` is pure relevance; `λ = 0` is pure diversity; default `0.5`.
//!
//! Input: a single relation `[item, relevance, vector]`, where `relevance` is a
//! number (ideally normalised to ~[0,1], e.g. a cosine similarity or RRF score)
//! and `vector` is the item's embedding (`DataValue::Vec`). Output:
//! `[item, rank]`, the 1-based selection order. Option `k` (default 0 = all)
//! caps how many to select; `lambda` (default 0.5, clamped to [0,1]).

use std::collections::BTreeMap;

use miette::{bail, Result};
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, Vector};
use crate::fixed_rule::{FixedRule, FixedRulePayload};
use crate::parse::SourceSpan;
use crate::runtime::db::Poison;
use crate::runtime::temp_store::RegularTempStore;

pub(crate) struct MaximalMarginalRelevance;

impl FixedRule for MaximalMarginalRelevance {
    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &mut RegularTempStore,
        poison: Poison,
    ) -> Result<()> {
        let in_rel = payload.get_input(0)?;
        let lambda = payload.float_option("lambda", Some(0.5))?.clamp(0.0, 1.0);
        let k_opt = payload.non_neg_integer_option("k", Some(0))?; // 0 => select all

        // Collect candidates: (item, relevance, vector). All vectors must share a
        // dimension (cosine over differing dims is meaningless and ndarray's `dot`
        // would panic), so we reject a mismatch with a clear error rather than crash.
        let mut cands: Vec<(DataValue, f64, Vector)> = vec![];
        let mut dim: Option<usize> = None;
        for tuple in in_rel.iter()? {
            let tuple = tuple?;
            if tuple.len() != 3 {
                bail!(
                    "MaximalMarginalRelevance expects a 3-column input \
                     [item, relevance, vector], got a row of arity {}",
                    tuple.len()
                );
            }
            let mut it = tuple.into_iter();
            let item = it.next().unwrap();
            let relevance = match it.next().unwrap().get_float() {
                Some(f) if f.is_finite() => f,
                Some(_) => bail!("MaximalMarginalRelevance: relevance (column 2) must be finite"),
                None => bail!("MaximalMarginalRelevance: relevance (column 2) must be a number"),
            };
            let vector = match it.next().unwrap() {
                DataValue::Vec(v) => v,
                other => bail!(
                    "MaximalMarginalRelevance: vector (column 3) must be a vector, got {:?}",
                    other
                ),
            };
            let vlen = vector_len(&vector);
            match dim {
                None => dim = Some(vlen),
                Some(d) if d != vlen => bail!(
                    "MaximalMarginalRelevance: inconsistent vector dimensions ({} vs {}); \
                     all embeddings must share one dimension",
                    d,
                    vlen
                ),
                Some(_) => {}
            }
            cands.push((item, relevance, vector));
            poison.check()?;
        }

        let n = cands.len();
        let target = if k_opt == 0 { n } else { k_opt.min(n) };

        let mut selected: Vec<usize> = Vec::with_capacity(target);
        let mut remaining: Vec<usize> = (0..n).collect();

        while selected.len() < target && !remaining.is_empty() {
            let mut best_pos = 0usize; // position within `remaining`
            let mut best_score = f64::NEG_INFINITY;
            for (ri, &ci) in remaining.iter().enumerate() {
                // Max cosine similarity to anything already selected. With nothing
                // selected yet the diversity term is 0, so the first pick is simply
                // the most relevant. Once items are selected we take the true max
                // (seeded at -inf) so anti-correlated (negative-cosine) candidates
                // are correctly rewarded rather than clamped at a 0 floor.
                let max_sim = if selected.is_empty() {
                    0.0
                } else {
                    selected
                        .iter()
                        .map(|&sj| cosine_sim(&cands[ci].2, &cands[sj].2))
                        .fold(f64::NEG_INFINITY, f64::max)
                };
                let mmr = lambda * cands[ci].1 - (1.0 - lambda) * max_sim;
                if mmr > best_score {
                    best_score = mmr;
                    best_pos = ri;
                }
            }
            let chosen = remaining.remove(best_pos);
            selected.push(chosen);
            poison.check()?;
        }

        for (rank, &ci) in selected.iter().enumerate() {
            out.put(vec![cands[ci].0.clone(), DataValue::from((rank + 1) as i64)]);
        }
        Ok(())
    }

    fn arity(
        &self,
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        // Output is always [item, rank].
        Ok(2)
    }
}

fn vector_len(v: &Vector) -> usize {
    match v {
        Vector::F32(a) => a.len(),
        Vector::F64(a) => a.len(),
    }
}

/// Cosine similarity in `[-1, 1]` (1 = identical direction). Returns 0 for a
/// zero vector, mismatched precision, or mismatched length (treated as no
/// diversity penalty). The length guard is defense-in-depth: `run` already rejects
/// inconsistent dimensions up front, but it keeps `ndarray::dot` (which panics on
/// a length mismatch) safe regardless of caller.
fn cosine_sim(a: &Vector, b: &Vector) -> f64 {
    match (a, b) {
        (Vector::F32(x), Vector::F32(y)) => {
            if x.len() != y.len() {
                return 0.0;
            }
            let dot = x.dot(y) as f64;
            let nx = (x.dot(x) as f64).sqrt();
            let ny = (y.dot(y) as f64).sqrt();
            if nx == 0.0 || ny == 0.0 {
                0.0
            } else {
                dot / (nx * ny)
            }
        }
        (Vector::F64(x), Vector::F64(y)) => {
            if x.len() != y.len() {
                return 0.0;
            }
            let dot = x.dot(y);
            let nx = x.dot(x).sqrt();
            let ny = y.dot(y).sqrt();
            if nx == 0.0 || ny == 0.0 {
                0.0
            } else {
                dot / (nx * ny)
            }
        }
        _ => 0.0,
    }
}
