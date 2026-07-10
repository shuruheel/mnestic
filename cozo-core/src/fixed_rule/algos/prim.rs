/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use graph::prelude::{DirectedCsrGraph, DirectedDegrees, DirectedNeighborsWithValues, Graph};
use std::cmp::Reverse;
use std::collections::BTreeMap;

use miette::Diagnostic;
use miette::Result;
use ordered_float::OrderedFloat;
use priority_queue::PriorityQueue;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::{FixedRule, FixedRulePayload};
use crate::parse::SourceSpan;
use crate::runtime::db::Poison;
use crate::runtime::graph_projection::VariantSpec;
use crate::runtime::temp_store::RegularTempStore;

pub(crate) struct MinimumSpanningTreePrim;

impl FixedRule for MinimumSpanningTreePrim {
    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &mut RegularTempStore,
        poison: Poison,
    ) -> Result<()> {
        let (source, input_base) =
            payload.graph_input(0, VariantSpec::weighted(true, false), &poison)?;
        let graph = source.weighted()?;
        let indices = source.indices();
        let inv_indices = source.inv_indices();
        // The empty early-return sits inside the no-starting-input arm: a user who names a
        // starting node (or supplies an empty starting relation) must still get the loud
        // `starting_node_not_found` / `empty_starting` diagnostic, not a silent empty result.
        let starting = match payload.get_input(input_base) {
            Err(_) => {
                if indices.is_empty() {
                    return Ok(());
                }
                // Vertex 0 is an edge endpoint in any graph built from an edge relation alone,
                // but a projection's `nodes:` can seat an isolated vertex there, and Prim from
                // an isolated vertex spans nothing. Start from the lowest vertex with an edge;
                // when no vertex has one, the tree really is empty. (The build is undirected,
                // so out-degree is degree.)
                match (0..graph.node_count()).find(|n| graph.out_degree(*n) > 0) {
                    None => return Ok(()),
                    Some(n) => n,
                }
            }
            Ok(rel) => {
                let tuple = rel.iter()?.next().ok_or_else(|| {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("The provided starting nodes relation is empty")]
                    #[diagnostic(code(algo::empty_starting))]
                    struct EmptyStarting(#[label] SourceSpan);

                    EmptyStarting(rel.span())
                })??;
                let dv = &tuple[0];
                *inv_indices.get(dv).ok_or_else(|| {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("The requested starting node {0:?} is not found")]
                    #[diagnostic(code(algo::starting_node_not_found))]
                    struct StartingNodeNotFound(DataValue, #[label] SourceSpan);

                    StartingNodeNotFound(dv.clone(), rel.span())
                })?
            }
        };
        let msp = prim(graph, starting, poison)?;
        for (src, dst, cost) in msp {
            out.put(vec![
                indices[src as usize].clone(),
                indices[dst as usize].clone(),
                DataValue::from(cost as f64),
            ]);
        }
        Ok(())
    }

    fn arity(
        &self,
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(3)
    }

    fn supports_projection(&self) -> bool {
        true
    }
}

fn prim(
    graph: &DirectedCsrGraph<u32, (), f32>,
    starting: u32,
    poison: Poison,
) -> Result<Vec<(u32, u32, f32)>> {
    let mut visited = vec![false; graph.node_count() as usize];
    let mut mst_edges = Vec::with_capacity((graph.node_count() - 1) as usize);
    let mut pq = PriorityQueue::new();

    let mut relax_edges_at_node = |node: u32, pq: &mut PriorityQueue<_, _>| {
        visited[node as usize] = true;
        for target in graph.out_neighbors_with_values(node) {
            let to_node = target.target;
            let cost = target.value;
            if visited[to_node as usize] {
                continue;
            }
            pq.push_increase(to_node, (Reverse(OrderedFloat(cost)), node));
        }
    };

    relax_edges_at_node(starting, &mut pq);

    while let Some((to_node, (Reverse(OrderedFloat(cost)), from_node))) = pq.pop() {
        if mst_edges.len() == (graph.node_count() - 1) as usize {
            break;
        }
        mst_edges.push((from_node, to_node, cost));
        relax_edges_at_node(to_node, &mut pq);
        poison.check()?;
    }

    Ok(mst_edges)
}
