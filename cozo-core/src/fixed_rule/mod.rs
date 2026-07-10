/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::BTreeMap;
use std::sync::Arc;

use crossbeam::channel::{bounded, Receiver, Sender};
#[allow(unused_imports)]
use either::{Left, Right};
#[cfg(feature = "graph-algo")]
use graph::prelude::{CsrLayout, DirectedCsrGraph, GraphBuilder};
use itertools::Itertools;
use lazy_static::lazy_static;
use miette::IntoDiagnostic;
#[allow(unused_imports)]
use miette::{bail, ensure, Diagnostic, Report, Result};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::{
    FixedRuleOptionNotFoundError, MagicFixedRuleApply, MagicFixedRuleRuleArg, MagicSymbol,
    WrongFixedRuleOptionError,
};
use crate::data::symb::Symbol;
use crate::data::tuple::TupleIter;
use crate::data::value::DataValue;
#[cfg(feature = "graph-algo")]
use crate::fixed_rule::algos::*;
use crate::fixed_rule::utilities::*;
use crate::parse::SourceSpan;
use crate::runtime::db::Poison;
use crate::runtime::temp_store::{EpochStore, RegularTempStore};
use crate::runtime::transact::SessionTx;
use crate::NamedRows;

#[cfg(feature = "graph-algo")]
pub(crate) mod algos;
pub(crate) mod utilities;

/// Passed into implementation of fixed rule, can be used to obtain relation inputs and options
pub struct FixedRulePayload<'a, 'b> {
    pub(crate) manifest: &'a MagicFixedRuleApply,
    pub(crate) stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    pub(crate) tx: &'a SessionTx<'b>,
}

/// Represents an input relation during the execution of a fixed rule
#[derive(Copy, Clone)]
pub struct FixedRuleInputRelation<'a, 'b> {
    arg_manifest: &'a MagicFixedRuleRuleArg,
    stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    tx: &'a SessionTx<'b>,
}

impl<'a, 'b> FixedRuleInputRelation<'a, 'b> {
    /// The arity of the input relation
    pub fn arity(&self) -> Result<usize> {
        self.arg_manifest.arity(self.tx, self.stores)
    }
    /// Ensure the input relation contains tuples of the given minimal length.
    pub fn ensure_min_len(self, len: usize) -> Result<Self> {
        #[derive(Error, Diagnostic, Debug)]
        #[error("Input relation to algorithm has insufficient arity")]
        #[diagnostic(help("Arity should be at least {0} but is {1}"))]
        #[diagnostic(code(algo::input_relation_bad_arity))]
        struct InputRelationArityError(usize, usize, #[label] SourceSpan);

        let arity = self.arg_manifest.arity(self.tx, self.stores)?;
        ensure!(
            arity >= len,
            InputRelationArityError(len, arity, self.arg_manifest.span())
        );
        Ok(self)
    }
    /// Get the binding map of the input relation
    pub fn get_binding_map(&self, offset: usize) -> BTreeMap<Symbol, usize> {
        self.arg_manifest.get_binding_map(offset)
    }
    /// Iterate the input relation
    pub fn iter(&self) -> Result<TupleIter<'a>> {
        Ok(match &self.arg_manifest {
            MagicFixedRuleRuleArg::InMem { name, .. } => {
                let store = self.stores.get(name).ok_or_else(|| {
                    RuleNotFoundError(name.symbol().to_string(), name.symbol().span)
                })?;
                Box::new(store.all_iter().map(|t| Ok(t.into_tuple())))
            }
            MagicFixedRuleRuleArg::Stored {
                name,
                valid_at,
                tx_valid_at,
                ..
            } => {
                let relation = self.tx.get_relation(name, false)?;
                if let Some(tt) = tx_valid_at {
                    // bitemporal input (mnestic fork, 4b): two-level scan
                    Box::new(relation.bitemporal_scan_all(self.tx, *valid_at, *tt))
                } else if let Some(valid_at) = valid_at {
                    Box::new(relation.skip_scan_all(self.tx, *valid_at))
                } else {
                    Box::new(relation.scan_all(self.tx))
                }
            }
        })
    }
    /// Iterate the relation with the given single-value prefix
    pub fn prefix_iter(&self, prefix: &DataValue) -> Result<TupleIter<'_>> {
        Ok(match self.arg_manifest {
            MagicFixedRuleRuleArg::InMem { name, .. } => {
                let store = self.stores.get(name).ok_or_else(|| {
                    RuleNotFoundError(name.symbol().to_string(), name.symbol().span)
                })?;
                let t = vec![prefix.clone()];
                Box::new(store.prefix_iter(&t).map(|t| Ok(t.into_tuple())))
            }
            MagicFixedRuleRuleArg::Stored {
                name,
                valid_at,
                tx_valid_at,
                ..
            } => {
                let relation = self.tx.get_relation(name, false)?;
                let t = vec![prefix.clone()];
                if let Some(tt) = tx_valid_at {
                    // bitemporal input (mnestic fork, 4b): two-level scan
                    Box::new(relation.bitemporal_scan_prefix(self.tx, &t, *valid_at, *tt))
                } else if let Some(valid_at) = valid_at {
                    Box::new(relation.skip_scan_prefix(self.tx, &t, *valid_at))
                } else {
                    Box::new(relation.scan_prefix(self.tx, &t))
                }
            }
        })
    }
    /// Get the source span of the input relation. Useful for generating informative error messages.
    pub fn span(&self) -> SourceSpan {
        self.arg_manifest.span()
    }
    /// Convert the input relation into a directed graph.
    /// If `undirected` is true, then each edge in the input relation is treated as a pair
    /// of edges, one for each direction.
    ///
    /// Returns the graph, the vertices in a vector with the index the same as used in the graph,
    /// and the inverse vertex mapping.
    ///
    /// Equivalent to [`Self::as_directed_graph_checked`] with no node relation and a poison that
    /// never fires.
    #[cfg(feature = "graph-algo")]
    pub fn as_directed_graph(
        &self,
        undirected: bool,
    ) -> Result<(
        DirectedCsrGraph<u32>,
        Vec<DataValue>,
        BTreeMap<DataValue, u32>,
    )> {
        self.as_directed_graph_checked(undirected, None, &Poison::default())
    }
    /// Convert the input relation into a directed graph, checking `poison` as the tuples are
    /// scanned so that a long build can be killed, and optionally registering the vertices of
    /// `nodes` before the edges are read.
    ///
    /// A vertex that appears in `nodes` but in no edge becomes a real degree-0 vertex: it is
    /// counted by `node_count` and enumerated by the algorithms. Without `nodes` the vertex set
    /// is exactly the set of edge endpoints.
    ///
    /// Returns the graph, the vertices in a vector with the index the same as used in the graph,
    /// and the inverse vertex mapping.
    #[cfg(feature = "graph-algo")]
    pub fn as_directed_graph_checked(
        &self,
        undirected: bool,
        nodes: Option<&FixedRuleInputRelation<'_, '_>>,
        poison: &Poison,
    ) -> Result<(
        DirectedCsrGraph<u32>,
        Vec<DataValue>,
        BTreeMap<DataValue, u32>,
    )> {
        let mut indices: Vec<DataValue> = vec![];
        let mut inv_indices: BTreeMap<DataValue, u32> = Default::default();
        register_nodes(nodes, &mut indices, &mut inv_indices, poison)?;

        let mut edges: Vec<(u32, u32)> = vec![];
        for (i, r_tuple) in self.iter()?.enumerate() {
            let mut tuple = r_tuple?.into_iter();
            let from = tuple.next().ok_or_else(|| NotAnEdgeError(self.span()))?;
            let to = tuple.next().ok_or_else(|| NotAnEdgeError(self.span()))?;
            let from_idx = intern_node(&mut indices, &mut inv_indices, from);
            let to_idx = intern_node(&mut indices, &mut inv_indices, to);
            edges.push((from_idx, to_idx));
            if i & (GRAPH_BUILD_POISON_INTERVAL - 1) == 0 {
                poison.check()?;
            }
        }
        check_csr_capacity(edges.len(), undirected, indices.len(), self.span())?;

        let node_count = nodes.map(|_| indices.len());
        let it = if undirected {
            Right(edges.into_iter().flat_map(|(f, t)| [(f, t), (t, f)]))
        } else {
            Left(edges.into_iter())
        };
        let builder = GraphBuilder::new().csr_layout(CsrLayout::Sorted).edges(it);
        let graph: DirectedCsrGraph<u32> = match node_count {
            Some(n) if n > 0 => builder.node_values(vec![(); n]).build(),
            _ => builder.build(),
        };
        Ok((graph, indices, inv_indices))
    }
    /// Convert the input relation into a directed weighted graph.
    /// If `undirected` is true, then each edge in the input relation is treated as a pair
    /// of edges, one for each direction.
    ///
    /// Returns the graph, the vertices in a vector with the index the same as used in the graph,
    /// and the inverse vertex mapping.
    ///
    /// Equivalent to [`Self::as_directed_weighted_graph_checked`] with no node relation and a
    /// poison that never fires.
    #[cfg(feature = "graph-algo")]
    pub fn as_directed_weighted_graph(
        &self,
        undirected: bool,
        allow_negative_weights: bool,
    ) -> Result<(
        DirectedCsrGraph<u32, (), f32>,
        Vec<DataValue>,
        BTreeMap<DataValue, u32>,
    )> {
        self.as_directed_weighted_graph_checked(
            undirected,
            allow_negative_weights,
            None,
            &Poison::default(),
        )
    }
    /// The interruptible, optionally node-registering counterpart of
    /// [`Self::as_directed_weighted_graph`]. See [`Self::as_directed_graph_checked`] for what
    /// `nodes` and `poison` do.
    #[cfg(feature = "graph-algo")]
    pub fn as_directed_weighted_graph_checked(
        &self,
        undirected: bool,
        allow_negative_weights: bool,
        nodes: Option<&FixedRuleInputRelation<'_, '_>>,
        poison: &Poison,
    ) -> Result<(
        DirectedCsrGraph<u32, (), f32>,
        Vec<DataValue>,
        BTreeMap<DataValue, u32>,
    )> {
        let (graph, indices, inv_indices, _has_negative) =
            self.build_weighted_graph(undirected, allow_negative_weights, nodes, poison)?;
        Ok((graph, indices, inv_indices))
    }
    /// The weighted builder proper. Additionally reports whether any edge weight was negative:
    /// the graph projection cache builds permissively and records this, so that a strict
    /// consumer of a shared variant can reject it (`docs/specs/graph-projection.md` §3.2.2).
    #[cfg(feature = "graph-algo")]
    pub(crate) fn build_weighted_graph(
        &self,
        undirected: bool,
        allow_negative_weights: bool,
        nodes: Option<&FixedRuleInputRelation<'_, '_>>,
        poison: &Poison,
    ) -> Result<(
        DirectedCsrGraph<u32, (), f32>,
        Vec<DataValue>,
        BTreeMap<DataValue, u32>,
        bool,
    )> {
        let mut indices: Vec<DataValue> = vec![];
        let mut inv_indices: BTreeMap<DataValue, u32> = Default::default();
        register_nodes(nodes, &mut indices, &mut inv_indices, poison)?;

        let weight_span = || {
            self.arg_manifest
                .bindings()
                .get(2)
                .map(|s| s.span)
                .unwrap_or_else(|| self.span())
        };

        let mut has_negative = false;
        let mut edges: Vec<(u32, u32, f32)> = vec![];
        for (i, r_tuple) in self.iter()?.enumerate() {
            let mut tuple = r_tuple?.into_iter();
            let from = tuple.next().ok_or_else(|| NotAnEdgeError(self.span()))?;
            let to = tuple.next().ok_or_else(|| NotAnEdgeError(self.span()))?;
            let from_idx = intern_node(&mut indices, &mut inv_indices, from);
            let to_idx = intern_node(&mut indices, &mut inv_indices, to);
            let weight = match tuple.next() {
                None => 1.0,
                Some(d) => {
                    let f = match d.get_float() {
                        Some(f) if f.is_finite() => f,
                        _ => bail!(BadEdgeWeightError(d, weight_span())),
                    };
                    if f < 0. {
                        ensure!(allow_negative_weights, BadEdgeWeightError(d, weight_span()));
                        has_negative = true;
                    }
                    f
                }
            };
            edges.push((from_idx, to_idx, weight as f32));
            if i & (GRAPH_BUILD_POISON_INTERVAL - 1) == 0 {
                poison.check()?;
            }
        }
        check_csr_capacity(edges.len(), undirected, indices.len(), self.span())?;

        let node_count = nodes.map(|_| indices.len());
        let it = if undirected {
            Right(
                edges
                    .into_iter()
                    .flat_map(|(f, t, w)| [(f, t, w), (t, f, w)]),
            )
        } else {
            Left(edges.into_iter())
        };
        let builder = GraphBuilder::new()
            .csr_layout(CsrLayout::Sorted)
            .edges_with_values(it);
        let graph: DirectedCsrGraph<u32, (), f32> = match node_count {
            Some(n) if n > 0 => builder.node_values(vec![(); n]).build(),
            _ => builder.build(),
        };
        Ok((graph, indices, inv_indices, has_negative))
    }
}

/// Batched cadence for [`Poison`] checks while a fixed rule scans tuples into a CSR — the same
/// power-of-two mask the relational-algebra pipeline uses (`query/ra.rs`).
#[cfg(feature = "graph-algo")]
const GRAPH_BUILD_POISON_INTERVAL: usize = 4096;

/// Map a vertex value to its dense `u32` id, assigning the next id if it is new.
#[cfg(feature = "graph-algo")]
#[inline]
fn intern_node(
    indices: &mut Vec<DataValue>,
    inv_indices: &mut BTreeMap<DataValue, u32>,
    node: DataValue,
) -> u32 {
    if let Some(idx) = inv_indices.get(&node) {
        *idx
    } else {
        let idx = indices.len() as u32;
        inv_indices.insert(node.clone(), idx);
        indices.push(node);
        idx
    }
}

/// Give every vertex of `nodes` an id before any edge is read, so that ids `0..nodes.len()`
/// name exactly the declared vertices and the isolated ones survive into the CSR.
#[cfg(feature = "graph-algo")]
fn register_nodes(
    nodes: Option<&FixedRuleInputRelation<'_, '_>>,
    indices: &mut Vec<DataValue>,
    inv_indices: &mut BTreeMap<DataValue, u32>,
    poison: &Poison,
) -> Result<()> {
    let Some(nodes) = nodes else { return Ok(()) };
    for (i, tuple) in nodes.iter()?.enumerate() {
        let node = tuple?
            .into_iter()
            .next()
            .ok_or_else(|| NotANodeError(nodes.span()))?;
        intern_node(indices, inv_indices, node);
        if i & (GRAPH_BUILD_POISON_INTERVAL - 1) == 0 {
            poison.check()?;
        }
    }
    Ok(())
}

/// `DirectedCsrGraph<u32>` indexes both its vertices and its CSR offsets with `u32`, so the
/// post-doubling edge count and the vertex count must each stay below `u32::MAX`. Checked before
/// the build rather than after, because the build allocates proportionally to both.
#[cfg(feature = "graph-algo")]
fn check_csr_capacity(
    n_edges: usize,
    undirected: bool,
    n_nodes: usize,
    span: SourceSpan,
) -> Result<()> {
    let n_edges = n_edges as u64 * if undirected { 2 } else { 1 };
    ensure!(
        n_edges < u32::MAX as u64,
        GraphTooLargeError("edges", n_edges, span)
    );
    ensure!(
        (n_nodes as u64) < u32::MAX as u64,
        GraphTooLargeError("vertices", n_nodes as u64, span)
    );
    Ok(())
}

impl<'a, 'b> FixedRulePayload<'a, 'b> {
    /// Get the total number of input relations.
    pub fn inputs_count(&self) -> usize {
        self.manifest.relations_count()
    }
    /// Get the input relation at `idx`.
    pub fn get_input(&self, idx: usize) -> Result<FixedRuleInputRelation<'a, 'b>> {
        let arg_manifest = self.manifest.relation(idx)?;
        Ok(FixedRuleInputRelation {
            arg_manifest,
            stores: self.stores,
            tx: self.tx,
        })
    }
    /// Get the name of the current fixed rule
    pub fn name(&self) -> &str {
        &self.manifest.fixed_handle.name
    }
    /// Get the source span of the payloads. Useful for generating informative errors.
    pub fn span(&self) -> SourceSpan {
        self.manifest.span
    }
    /// Extract an expression option
    pub fn expr_option(&self, name: &str, default: Option<Expr>) -> Result<Expr> {
        match self.manifest.options.get(name) {
            Some(ex) => Ok(ex.clone()),
            None => match default {
                Some(ex) => Ok(ex),
                None => Err(FixedRuleOptionNotFoundError {
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                }
                .into()),
            },
        }
    }

    /// Extract a string option
    pub fn string_option(
        &self,
        name: &str,
        default: Option<&str>,
    ) -> Result<SmartString<LazyCompact>> {
        match self.manifest.options.get(name) {
            Some(ex) => match ex.clone().eval_to_const()? {
                DataValue::Str(s) => Ok(s),
                _ => Err(WrongFixedRuleOptionError {
                    name: name.to_string(),
                    span: ex.span(),
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                    help: "a string is required".to_string(),
                }
                .into()),
            },
            None => match default {
                None => Err(FixedRuleOptionNotFoundError {
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                }
                .into()),
                Some(s) => Ok(SmartString::from(s)),
            },
        }
    }

    /// Get the source span of the named option. Useful for generating informative error messages.
    pub fn option_span(&self, name: &str) -> Result<SourceSpan> {
        match self.manifest.options.get(name) {
            None => Err(FixedRuleOptionNotFoundError {
                name: name.to_string(),
                span: self.manifest.span,
                rule_name: self.manifest.fixed_handle.name.to_string(),
            }
            .into()),
            Some(v) => Ok(v.span()),
        }
    }
    /// Extract an integer option
    pub fn integer_option(&self, name: &str, default: Option<i64>) -> Result<i64> {
        match self.manifest.options.get(name) {
            Some(v) => match v.clone().eval_to_const() {
                Ok(DataValue::Num(n)) => match n.get_int() {
                    Some(i) => Ok(i),
                    None => Err(FixedRuleOptionNotFoundError {
                        name: name.to_string(),
                        span: self.manifest.span,
                        rule_name: self.manifest.fixed_handle.name.to_string(),
                    }
                    .into()),
                },
                _ => Err(WrongFixedRuleOptionError {
                    name: name.to_string(),
                    span: v.span(),
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                    help: "an integer is required".to_string(),
                }
                .into()),
            },
            None => match default {
                Some(v) => Ok(v),
                None => Err(FixedRuleOptionNotFoundError {
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                }
                .into()),
            },
        }
    }
    /// Extract a positive integer option
    pub fn pos_integer_option(&self, name: &str, default: Option<usize>) -> Result<usize> {
        let i = self.integer_option(name, default.map(|i| i as i64))?;
        ensure!(
            i > 0,
            WrongFixedRuleOptionError {
                name: name.to_string(),
                span: self.option_span(name)?,
                rule_name: self.manifest.fixed_handle.name.to_string(),
                help: "a positive integer is required".to_string(),
            }
        );
        Ok(i as usize)
    }
    /// Extract a non-negative integer option
    pub fn non_neg_integer_option(&self, name: &str, default: Option<usize>) -> Result<usize> {
        let i = self.integer_option(name, default.map(|i| i as i64))?;
        ensure!(
            i >= 0,
            WrongFixedRuleOptionError {
                name: name.to_string(),
                span: self.option_span(name)?,
                rule_name: self.manifest.fixed_handle.name.to_string(),
                help: "a non-negative integer is required".to_string(),
            }
        );
        Ok(i as usize)
    }
    /// Extract a floating point option
    pub fn float_option(&self, name: &str, default: Option<f64>) -> Result<f64> {
        match self.manifest.options.get(name) {
            Some(v) => match v.clone().eval_to_const() {
                Ok(DataValue::Num(n)) => {
                    let f = n.get_float();
                    Ok(f)
                }
                _ => Err(WrongFixedRuleOptionError {
                    name: name.to_string(),
                    span: v.span(),
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                    help: "a floating number is required".to_string(),
                }
                .into()),
            },
            None => match default {
                Some(v) => Ok(v),
                None => Err(FixedRuleOptionNotFoundError {
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                }
                .into()),
            },
        }
    }
    /// Extract a floating point option between 0. and 1.
    pub fn unit_interval_option(&self, name: &str, default: Option<f64>) -> Result<f64> {
        let f = self.float_option(name, default)?;
        ensure!(
            (0. ..=1.).contains(&f),
            WrongFixedRuleOptionError {
                name: name.to_string(),
                span: self.option_span(name)?,
                rule_name: self.manifest.fixed_handle.name.to_string(),
                help: "a number between 0. and 1. is required".to_string(),
            }
        );
        Ok(f)
    }
    /// Extract a boolean option
    pub fn bool_option(&self, name: &str, default: Option<bool>) -> Result<bool> {
        match self.manifest.options.get(name) {
            Some(v) => match v.clone().eval_to_const() {
                Ok(DataValue::Bool(b)) => Ok(b),
                _ => Err(WrongFixedRuleOptionError {
                    name: name.to_string(),
                    span: v.span(),
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                    help: "a boolean value is required".to_string(),
                }
                .into()),
            },
            None => match default {
                Some(v) => Ok(v),
                None => Err(FixedRuleOptionNotFoundError {
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                }
                .into()),
            },
        }
    }
}

/// Trait for an implementation of an algorithm or a utility
pub trait FixedRule: Send + Sync {
    /// Called to initialize the options given.
    /// Will always be called once, before anything else.
    /// You can mutate the options if you need to.
    /// The default implementation does nothing.
    fn init_options(
        &self,
        _options: &mut BTreeMap<SmartString<LazyCompact>, Expr>,
        _span: SourceSpan,
    ) -> Result<()> {
        Ok(())
    }
    /// You must return the row width of the returned relation and it must be accurate.
    /// This function may be called multiple times.
    fn arity(
        &self,
        options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        rule_head: &[Symbol],
        span: SourceSpan,
    ) -> Result<usize>;
    /// You should implement the logic of your algorithm/utility in this function.
    /// The outputs are written to `out`. You should check `poison` periodically
    /// for user-initiated termination.
    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &'_ mut RegularTempStore,
        poison: Poison,
    ) -> Result<()>;
}

/// Simple wrapper for custom fixed rule. You have less control than implementing [FixedRule] directly,
/// but implementation is simpler.
pub struct SimpleFixedRule {
    return_arity: usize,
    rule: Box<
        dyn Fn(Vec<NamedRows>, BTreeMap<String, DataValue>) -> Result<NamedRows>
            + Send
            + Sync
            + 'static,
    >,
}

impl SimpleFixedRule {
    /// Construct a SimpleFixedRule.
    ///
    /// * `return_arity`: The return arity of this rule.
    /// * `rule`:  The rule implementation as a closure.
    //    The first argument is a vector of input relations, realized into NamedRows,
    //    and the second argument is a JSON object of passed in options.
    //    The returned NamedRows is the return relation of the application of this rule.
    //    Every row of the returned relation must have length equal to `return_arity`.
    pub fn new<R>(return_arity: usize, rule: R) -> Self
    where
        R: Fn(Vec<NamedRows>, BTreeMap<String, DataValue>) -> Result<NamedRows>
            + Send
            + Sync
            + 'static,
    {
        Self {
            return_arity,
            rule: Box::new(rule),
        }
    }
    /// Construct a SimpleFixedRule that uses channels for communication.
    pub fn rule_with_channel(
        return_arity: usize,
    ) -> (
        Self,
        Receiver<(
            Vec<NamedRows>,
            BTreeMap<String, DataValue>,
            Sender<Result<NamedRows>>,
        )>,
    ) {
        let (db2app_sender, db2app_receiver) = bounded(0);
        (
            Self {
                return_arity,
                rule: Box::new(move |inputs, options| -> Result<NamedRows> {
                    let (app2db_sender, app2db_receiver) = bounded(0);
                    db2app_sender
                        .send((inputs, options, app2db_sender))
                        .into_diagnostic()?;
                    app2db_receiver.recv().into_diagnostic()?
                }),
            },
            db2app_receiver,
        )
    }
}

impl FixedRule for SimpleFixedRule {
    fn arity(
        &self,
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(self.return_arity)
    }

    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &'_ mut RegularTempStore,
        _poison: Poison,
    ) -> Result<()> {
        let options: BTreeMap<_, _> = payload
            .manifest
            .options
            .iter()
            .map(|(k, v)| -> Result<_> {
                let val = v.clone().eval_to_const()?;
                Ok((k.to_string(), val))
            })
            .try_collect()?;
        let input_arity = payload.manifest.rule_args.len();
        let inputs: Vec<_> = (0..input_arity)
            .map(|i| -> Result<_> {
                let input = payload.get_input(i).unwrap();
                let rows: Vec<_> = input.iter()?.try_collect()?;
                let mut headers = input
                    .arg_manifest
                    .bindings()
                    .iter()
                    .map(|s| s.name.to_string())
                    .collect_vec();
                let l = headers.len();
                let m = input.arg_manifest.arity(payload.tx, payload.stores)?;
                for i in l..m {
                    headers.push(format!("_{i}"));
                }
                Ok(NamedRows::new(headers, rows))
            })
            .try_collect()?;
        let results: NamedRows = (self.rule)(inputs, options)?;
        for row in results.rows {
            #[derive(Debug, Error, Diagnostic)]
            #[error("arity mismatch: expect {1}, got {2}")]
            #[diagnostic(code(parser::simple_fixed_rule_arity_mismatch))]
            struct ArityMismatch(#[label] SourceSpan, usize, usize);

            ensure!(
                row.len() == self.return_arity,
                ArityMismatch(payload.span(), self.return_arity, row.len())
            );
            out.put(row);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Cannot determine arity for algo {0} since {1}")]
#[diagnostic(code(parser::no_algo_arity))]
pub(crate) struct CannotDetermineArity(
    pub(crate) String,
    pub(crate) String,
    #[label] pub(crate) SourceSpan,
);

#[derive(Clone, Debug)]
pub(crate) struct FixedRuleHandle {
    pub(crate) name: Symbol,
}

lazy_static! {
    pub(crate) static ref DEFAULT_FIXED_RULES: BTreeMap<String, Arc<Box<dyn FixedRule>>> = {
        BTreeMap::from([
            #[cfg(feature = "graph-algo")]
            (
                "ClusteringCoefficients".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(ClusteringCoefficients)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "DegreeCentrality".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(DegreeCentrality)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "ClosenessCentrality".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(ClosenessCentrality)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "BetweennessCentrality".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(BetweennessCentrality)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "DepthFirstSearch".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(Dfs)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "DFS".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(Dfs)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "BreadthFirstSearch".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(Bfs)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "BFS".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(Bfs)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "ShortestPathBFS".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(ShortestPathBFS)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "ShortestPathDijkstra".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(ShortestPathDijkstra)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "ShortestPathAStar".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(ShortestPathAStar)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "KShortestPathYen".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(KShortestPathYen)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "MinimumSpanningTreePrim".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(MinimumSpanningTreePrim)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "MinimumSpanningForestKruskal".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(MinimumSpanningForestKruskal)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "TopSort".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(TopSort)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "ConnectedComponents".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(StronglyConnectedComponent::new(false))),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "StronglyConnectedComponents".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(StronglyConnectedComponent::new(true))),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "SCC".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(StronglyConnectedComponent::new(true))),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "PageRank".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(PageRank)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "CommunityDetectionLouvain".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(CommunityDetectionLouvain)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "LabelPropagation".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(LabelPropagation)),
            ),
            #[cfg(feature = "graph-algo")]
            (
                "RandomWalk".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(RandomWalk)),
            ),
            (
                "ReorderSort".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(ReorderSort)),
            ),
            (
                "ReciprocalRankFusion".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(ReciprocalRankFusion)),
            ),
            (
                "RRF".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(ReciprocalRankFusion)),
            ),
            (
                "MaximalMarginalRelevance".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(MaximalMarginalRelevance)),
            ),
            (
                "MMR".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(MaximalMarginalRelevance)),
            ),
            (
                "JsonReader".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(JsonReader)),
            ),
            (
                "CsvReader".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(CsvReader)),
            ),
            (
                "Constant".to_string(),
                Arc::<Box<dyn FixedRule>>::new(Box::new(Constant)),
            ),
        ])
    };
}

impl FixedRuleHandle {
    pub(crate) fn new(name: &str, span: SourceSpan) -> Self {
        FixedRuleHandle {
            name: Symbol::new(name, span),
        }
    }
}

#[derive(Error, Diagnostic, Debug)]
#[error("The relation cannot be interpreted as an edge")]
#[diagnostic(code(algo::not_an_edge))]
#[diagnostic(help("Edge relation requires tuples of length at least two"))]
struct NotAnEdgeError(#[label] SourceSpan);

#[cfg(feature = "graph-algo")]
#[derive(Error, Diagnostic, Debug)]
#[error("The relation cannot be interpreted as a relation of nodes")]
#[diagnostic(code(algo::not_a_node))]
#[diagnostic(help("A node relation requires tuples of length at least one"))]
struct NotANodeError(#[label] SourceSpan);

#[cfg(feature = "graph-algo")]
#[derive(Error, Diagnostic, Debug)]
#[error("The graph has too many {0} ({1}) to be represented")]
#[diagnostic(code(algo::graph_too_large))]
#[diagnostic(help(
    "The compressed adjacency representation indexes vertices and edges with 32-bit integers"
))]
struct GraphTooLargeError(&'static str, u64, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error(
    "The value {0:?} at the third position in the relation cannot be interpreted as edge weights"
)]
#[diagnostic(code(algo::invalid_edge_weight))]
#[diagnostic(help(
    "Edge weights must be finite numbers. Some algorithm also requires positivity."
))]
struct BadEdgeWeightError(DataValue, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("The requested rule '{0}' cannot be found")]
#[diagnostic(code(algo::rule_not_found))]
struct RuleNotFoundError(String, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("Invalid reverse scanning of triples")]
#[diagnostic(code(algo::invalid_reverse_triple_scan))]
#[diagnostic(help(
    "Inverse scanning of triples requires the type to be 'ref', or the value be indexed"
))]
struct InvalidInverseTripleUse(String, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("Required node with key {missing:?} not found")]
#[diagnostic(code(algo::node_with_key_not_found))]
#[diagnostic(help(
    "The relation is interpreted as a relation of nodes, but the required key is missing"
))]
pub(crate) struct NodeNotFoundError {
    pub(crate) missing: DataValue,
    #[label]
    pub(crate) span: SourceSpan,
}

#[derive(Error, Diagnostic, Debug)]
#[error("Unacceptable value {0:?} encountered")]
#[diagnostic(code(algo::unacceptable_value))]
pub(crate) struct BadExprValueError(
    pub(crate) DataValue,
    #[label] pub(crate) SourceSpan,
    #[help] pub(crate) String,
);

#[derive(Error, Diagnostic, Debug)]
#[error("The requested fixed rule '{0}' is not found")]
#[diagnostic(code(parser::fixed_rule_not_found))]
pub(crate) struct FixedRuleNotFoundError(pub(crate) String, #[label] pub(crate) SourceSpan);

impl MagicFixedRuleRuleArg {
    pub(crate) fn arity(
        &self,
        tx: &SessionTx<'_>,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<usize> {
        Ok(match self {
            MagicFixedRuleRuleArg::InMem { name, .. } => {
                let store = stores.get(name).ok_or_else(|| {
                    RuleNotFoundError(name.symbol().to_string(), name.symbol().span)
                })?;
                store.arity
            }
            MagicFixedRuleRuleArg::Stored { name, .. } => {
                let handle = tx.get_relation(name, false)?;
                handle.arity()
            }
        })
    }
}

#[cfg(all(test, feature = "graph-algo"))]
mod tests {
    use super::check_csr_capacity;
    use crate::parse::SourceSpan;

    /// The u32 capacity guard is the only barrier against `indices.len() as u32` wrapping and
    /// silently aliasing distinct vertices — the boundary is unreachable by any end-to-end test,
    /// so it is pinned here at the exact edges.
    #[test]
    fn csr_capacity_guard_boundaries() {
        let span = SourceSpan(0, 0);
        let max = u32::MAX as usize;

        assert!(check_csr_capacity(max, false, 0, span).is_err());
        assert!(check_csr_capacity(max - 1, false, 0, span).is_ok());

        // Undirected doubles the edge count before the check.
        assert!(check_csr_capacity(max / 2 + 1, true, 0, span).is_err());
        assert!(check_csr_capacity(max / 2, true, 0, span).is_ok());

        assert!(check_csr_capacity(0, false, max, span).is_err());
        assert!(check_csr_capacity(0, false, max - 1, span).is_ok());
    }
}
