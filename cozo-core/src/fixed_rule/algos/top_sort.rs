/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use graph::prelude::{DirectedCsrGraph, DirectedNeighbors, Graph};
use std::collections::BTreeMap;

use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::{FixedRule, FixedRulePayload};
use crate::parse::SourceSpan;
use crate::runtime::db::Poison;
use crate::runtime::graph_projection::VariantSpec;
use crate::runtime::temp_store::RegularTempStore;

pub(crate) struct TopSort;

impl FixedRule for TopSort {
    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &mut RegularTempStore,
        poison: Poison,
    ) -> Result<()> {
        let (source, _input_base) = payload.graph_input(0, VariantSpec::unweighted(false), &poison)?;
        let indices = source.indices();
        if indices.is_empty() {
            return Ok(());
        }

        let sorted = kahn_g(source.unweighted()?, poison)?;

        for (idx, val_id) in sorted.iter().enumerate() {
            let val = indices.get(*val_id as usize).unwrap();
            let tuple = vec![DataValue::from(idx as i64), val.clone()];
            out.put(tuple);
        }

        Ok(())
    }

    fn arity(
        &self,
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(2)
    }

    fn supports_projection(&self) -> bool {
        true
    }
}

pub(crate) fn kahn_g(graph: &DirectedCsrGraph<u32>, poison: Poison) -> Result<Vec<u32>> {
    let graph_size = graph.node_count();
    let mut in_degree = vec![0; graph_size as usize];
    for tos in 0..graph_size {
        for to in graph.out_neighbors(tos) {
            in_degree[*to as usize] += 1;
        }
    }
    let mut sorted = Vec::with_capacity(graph_size as usize);
    let mut pending = vec![];

    for (node, degree) in in_degree.iter().enumerate() {
        if *degree == 0 {
            pending.push(node as u32);
        }
    }

    while let Some(removed) = pending.pop() {
        sorted.push(removed);
        for nxt in graph.out_neighbors(removed) {
            in_degree[*nxt as usize] -= 1;
            if in_degree[*nxt as usize] == 0 {
                pending.push(*nxt);
            }
        }
        poison.check()?;
    }

    Ok(sorted)
}
