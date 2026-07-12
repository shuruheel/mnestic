/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::BTreeMap;
use std::sync::Arc;

use itertools::Itertools;
use miette::{bail, ensure, miette, Diagnostic, Result};
use ordered_float::OrderedFloat;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::program::InputProgram;
use crate::data::relation::VecElementType;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, ValidityTs};
use crate::fts::TokenizerConfig;
use crate::parse::expr::{build_expr, parse_string};
use crate::parse::query::parse_query;
use crate::parse::{ExtractSpan, Pairs, Rule, SourceSpan};
use crate::runtime::relation::AccessLevel;
use crate::{Expr, FixedRule};

#[derive(Debug)]
pub enum SysOp {
    Compact,
    ListColumns(Symbol),
    ListIndices(Symbol),
    ListRelations,
    ListRunning,
    ListFixedRules,
    KillRunning(u64),
    Explain(Box<InputProgram>),
    RemoveRelation(Vec<Symbol>),
    RenameRelation(Vec<(Symbol, Symbol)>),
    ShowTrigger(Symbol),
    SetTriggers(Symbol, Vec<String>, Vec<String>, Vec<String>),
    SetAccessLevel(Vec<Symbol>, AccessLevel),
    CreateIndex(Symbol, Symbol, Vec<Symbol>),
    CreateVectorIndex(HnswIndexConfig),
    CreateFtsIndex(FtsIndexConfig),
    CreateMinHashLshIndex(MinHashLshConfig),
    RemoveIndex(Symbol, Symbol),
    DescribeRelation(Symbol, SmartString<LazyCompact>),
    /// Delete tuples whose stored arity is shorter than the schema
    /// (truncated values from interrupted writes). Surgical alternative to
    /// dropping a database that fails integrity checks.
    RepairCorrupt(Symbol),
    /// Rebuild a relation's HNSW/FTS/LSH indexes in place from their stored
    /// manifests (mnestic fork, 0.12.1). See `runtime/reindex.rs`.
    Reindex(Symbol),
    /// mnestic fork, bitemporality step 5: full belief timeline of the given
    /// keys of a tt-stamped relation — (relation, keys, limit, offset)
    TtHistory(Symbol, Vec<Vec<DataValue>>, Option<usize>, Option<usize>),
    /// (relation, cutoff tt µs): drop superseded records below the cutoff,
    /// persist the gc floor
    TtHistoryGc(Symbol, i64),
    /// (relation, keys, unredacted): hard-delete every record of the keys
    /// (GDPR); audit row written in the same transaction
    TtEvict(Symbol, Vec<Vec<DataValue>>, bool),
    /// mnestic fork, graph projection: register `name` as a cached CSR over
    /// the `edges` relation, optionally with `nodes` naming the vertex set.
    /// Builds nothing — variants materialise on first algorithm use.
    CreateGraph {
        name: SmartString<LazyCompact>,
        edges: SmartString<LazyCompact>,
        nodes: Option<SmartString<LazyCompact>>,
    },
    /// mnestic fork, graph projection: forget a projection, freeing its CSRs
    DropGraph(SmartString<LazyCompact>),
    /// mnestic fork, graph projection: one row per built variant
    ListGraphs,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FtsIndexConfig {
    pub base_relation: SmartString<LazyCompact>,
    pub index_name: SmartString<LazyCompact>,
    pub extractor: String,
    pub tokenizer: TokenizerConfig,
    pub filters: Vec<TokenizerConfig>,
}

#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MinHashLshConfig {
    pub base_relation: SmartString<LazyCompact>,
    pub index_name: SmartString<LazyCompact>,
    pub extractor: String,
    pub tokenizer: TokenizerConfig,
    pub filters: Vec<TokenizerConfig>,
    pub n_gram: usize,
    pub n_perm: usize,
    pub false_positive_weight: OrderedFloat<f64>,
    pub false_negative_weight: OrderedFloat<f64>,
    pub target_threshold: OrderedFloat<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HnswIndexConfig {
    pub base_relation: SmartString<LazyCompact>,
    pub index_name: SmartString<LazyCompact>,
    pub vec_dim: usize,
    pub dtype: VecElementType,
    pub vec_fields: Vec<SmartString<LazyCompact>>,
    pub distance: HnswDistance,
    pub ef_construction: usize,
    pub m_neighbours: usize,
    pub index_filter: Option<String>,
    pub extend_candidates: bool,
    pub keep_pruned_connections: bool,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize,
)]
pub enum HnswDistance {
    L2,
    InnerProduct,
    Cosine,
}

#[derive(Debug, Diagnostic, Error)]
#[error("Cannot interpret {0} as process ID")]
#[diagnostic(code(parser::not_proc_id))]
struct ProcessIdError(String, #[label] SourceSpan);

pub(crate) fn parse_sys(
    mut src: Pairs<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    algorithms: &BTreeMap<String, Arc<Box<dyn FixedRule>>>,
    custom_aggrs: crate::data::aggr::CustomAggrRegistries<'_>,
    cur_vld: ValidityTs,
) -> Result<SysOp> {
    let inner = src.next().unwrap();
    Ok(match inner.as_rule() {
        Rule::history_op => {
            let mut in_inner = inner.into_inner();
            let rel = in_inner.next().unwrap();
            let rel_symbol = Symbol::new(rel.as_str(), rel.extract_span());
            let keys_expr = build_expr(in_inner.next().unwrap(), param_pool)?;
            let keys = sysop_keys(keys_expr)?;
            // limit/offset are bare `pos_int` tokens, not exprs: an expr
            // would greedily parse `2 -1` as the single limit `2 - 1`
            let mut limit = None;
            let mut offset = None;
            if let Some(p) = in_inner.next() {
                limit = Some(sysop_pos_int(&p, "limit")?);
            }
            if let Some(p) = in_inner.next() {
                offset = Some(sysop_pos_int(&p, "offset")?);
            }
            SysOp::TtHistory(rel_symbol, keys, limit, offset)
        }
        Rule::history_gc_op => {
            let mut in_inner = inner.into_inner();
            let rel = in_inner.next().unwrap();
            let rel_symbol = Symbol::new(rel.as_str(), rel.extract_span());
            let cutoff_expr = build_expr(in_inner.next().unwrap(), param_pool)?;
            let cutoff = cutoff_expr
                .eval_to_const()?
                .get_int()
                .ok_or_else(|| miette!("::history_gc cutoff must be an integer (µs)"))?;
            SysOp::TtHistoryGc(rel_symbol, cutoff)
        }
        Rule::evict_op => {
            let mut in_inner = inner.into_inner();
            let rel = in_inner.next().unwrap();
            let rel_symbol = Symbol::new(rel.as_str(), rel.extract_span());
            let keys_expr = build_expr(in_inner.next().unwrap(), param_pool)?;
            let keys = sysop_keys(keys_expr)?;
            let unredacted = in_inner.next().is_some();
            SysOp::TtEvict(rel_symbol, keys, unredacted)
        }
        Rule::compact_op => SysOp::Compact,
        Rule::running_op => SysOp::ListRunning,
        Rule::kill_op => {
            let i_expr = inner.into_inner().next().unwrap();
            let i_val = build_expr(i_expr, param_pool)?;
            let i_val = i_val.eval_to_const()?;
            let i_val = i_val
                .get_int()
                .ok_or_else(|| miette!("Process ID must be an integer"))?;
            SysOp::KillRunning(i_val as u64)
        }
        Rule::explain_op => {
            let prog = parse_query(
                inner.into_inner().next().unwrap().into_inner(),
                param_pool,
                algorithms,
                custom_aggrs,
                cur_vld,
            )?;
            SysOp::Explain(Box::new(prog))
        }
        Rule::describe_relation_op => {
            let mut inner = inner.into_inner();
            let rels_p = inner.next().unwrap();
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            let description = match inner.next() {
                None => Default::default(),
                Some(desc_p) => parse_string(desc_p)?,
            };
            SysOp::DescribeRelation(rel, description)
        }
        Rule::repair_corrupt_op => {
            let rels_p = inner.into_inner().next().unwrap();
            SysOp::RepairCorrupt(Symbol::new(rels_p.as_str(), rels_p.extract_span()))
        }
        Rule::reindex_op => {
            let rels_p = inner.into_inner().next().unwrap();
            SysOp::Reindex(Symbol::new(rels_p.as_str(), rels_p.extract_span()))
        }
        Rule::list_relations_op => SysOp::ListRelations,
        Rule::remove_relations_op => {
            let rel = inner
                .into_inner()
                .map(|rels_p| Symbol::new(rels_p.as_str(), rels_p.extract_span()))
                .collect_vec();

            SysOp::RemoveRelation(rel)
        }
        Rule::list_columns_op => {
            let rels_p = inner.into_inner().next().unwrap();
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            SysOp::ListColumns(rel)
        }
        Rule::list_indices_op => {
            let rels_p = inner.into_inner().next().unwrap();
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            SysOp::ListIndices(rel)
        }
        Rule::rename_relations_op => {
            let rename_pairs = inner
                .into_inner()
                .map(|pair| {
                    let mut src = pair.into_inner();
                    let rels_p = src.next().unwrap();
                    let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
                    let rels_p = src.next().unwrap();
                    let new_rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
                    (rel, new_rel)
                })
                .collect_vec();
            SysOp::RenameRelation(rename_pairs)
        }
        Rule::access_level_op => {
            let mut ps = inner.into_inner();
            let access_level = match ps.next().unwrap().as_str() {
                "normal" => AccessLevel::Normal,
                "protected" => AccessLevel::Protected,
                "read_only" => AccessLevel::ReadOnly,
                "hidden" => AccessLevel::Hidden,
                _ => unreachable!(),
            };
            let mut rels = vec![];
            for rel_p in ps {
                let rel = Symbol::new(rel_p.as_str(), rel_p.extract_span());
                rels.push(rel)
            }
            SysOp::SetAccessLevel(rels, access_level)
        }
        Rule::trigger_relation_show_op => {
            let rels_p = inner.into_inner().next().unwrap();
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            SysOp::ShowTrigger(rel)
        }
        Rule::trigger_relation_op => {
            let mut src = inner.into_inner();
            let rels_p = src.next().unwrap();
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            let mut puts = vec![];
            let mut rms = vec![];
            let mut replaces = vec![];
            for clause in src {
                let mut clause_inner = clause.into_inner();
                let op = clause_inner.next().unwrap();
                let script = clause_inner.next().unwrap();
                let script_str = script.as_str();
                parse_query(
                    script.into_inner(),
                    &Default::default(),
                    algorithms,
                    // triggers: custom aggregates unsupported (R0; bounded-
                    // meet registrations likewise) — validate against empty
                    // registries so ::set_triggers fails fast.
                    crate::data::aggr::CustomAggrRegistries {
                        meet: &Default::default(),
                        bounded: &Default::default(),
                    },
                    cur_vld,
                )?;
                match op.as_rule() {
                    Rule::trigger_put => puts.push(script_str.to_string()),
                    Rule::trigger_rm => rms.push(script_str.to_string()),
                    Rule::trigger_replace => replaces.push(script_str.to_string()),
                    r => unreachable!("{:?}", r),
                }
            }
            SysOp::SetTriggers(rel, puts, rms, replaces)
        }
        Rule::lsh_idx_op => {
            let inner = inner.into_inner().next().unwrap();
            match inner.as_rule() {
                Rule::index_create_adv => {
                    let mut inner = inner.into_inner();
                    let rel = inner.next().unwrap();
                    let name = inner.next().unwrap();
                    let mut filters = vec![];
                    let mut tokenizer = TokenizerConfig {
                        name: Default::default(),
                        args: Default::default(),
                    };
                    let mut extractor = "".to_string();
                    let mut extract_filter = "".to_string();
                    let mut n_gram = 1;
                    let mut n_perm = 200;
                    let mut target_threshold = 0.9;
                    let mut false_positive_weight = 1.0;
                    let mut false_negative_weight = 1.0;
                    for opt_pair in inner {
                        let mut opt_inner = opt_pair.into_inner();
                        let opt_name = opt_inner.next().unwrap();
                        let opt_val = opt_inner.next().unwrap();
                        match opt_name.as_str() {
                            "false_positive_weight" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                false_positive_weight = v.get_float().ok_or_else(|| {
                                    miette!("false_positive_weight must be a float")
                                })?;
                            }
                            "false_negative_weight" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                false_negative_weight = v.get_float().ok_or_else(|| {
                                    miette!("false_negative_weight must be a float")
                                })?;
                            }
                            "n_gram" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                n_gram = v
                                    .get_int()
                                    .ok_or_else(|| miette!("n_gram must be an integer"))?
                                    as usize;
                            }
                            "n_perm" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                n_perm = v
                                    .get_int()
                                    .ok_or_else(|| miette!("n_perm must be an integer"))?
                                    as usize;
                            }
                            "target_threshold" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                target_threshold = v
                                    .get_float()
                                    .ok_or_else(|| miette!("target_threshold must be a float"))?;
                            }
                            "extractor" => {
                                let mut ex = build_expr(opt_val, param_pool)?;
                                ex.partial_eval()?;
                                extractor = ex.to_string();
                            }
                            "extract_filter" => {
                                let mut ex = build_expr(opt_val, param_pool)?;
                                ex.partial_eval()?;
                                extract_filter = ex.to_string();
                            }
                            "tokenizer" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                match expr {
                                    Expr::UnboundApply { op, args, .. } => {
                                        let mut targs = vec![];
                                        for arg in args.iter() {
                                            let v = arg.clone().eval_to_const()?;
                                            targs.push(v);
                                        }
                                        tokenizer.name = op;
                                        tokenizer.args = targs;
                                    }
                                    Expr::Binding { var, .. } => {
                                        tokenizer.name = var.name;
                                        tokenizer.args = vec![];
                                    }
                                    _ => bail!("Tokenizer must be a symbol or a call for an existing tokenizer"),
                                }
                            }
                            "filters" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                match expr {
                                    Expr::Apply { op, args, .. } => {
                                        if op.name != "OP_LIST" {
                                            bail!("Filters must be a list of filters");
                                        }
                                        for arg in args.iter() {
                                            match arg {
                                                Expr::UnboundApply { op, args, .. } => {
                                                    let mut targs = vec![];
                                                    for arg in args.iter() {
                                                        let v = arg.clone().eval_to_const()?;
                                                        targs.push(v);
                                                    }
                                                    filters.push(TokenizerConfig {
                                                        name: op.clone(),
                                                        args: targs,
                                                    })
                                                }
                                                Expr::Binding { var, .. } => {
                                                    filters.push(TokenizerConfig {
                                                        name: var.name.clone(),
                                                        args: vec![],
                                                    })
                                                }
                                                _ => bail!("Tokenizer must be a symbol or a call for an existing tokenizer"),
                                            }
                                        }
                                    }
                                    _ => bail!("Filters must be a list of filters"),
                                }
                            }
                            _ => bail!("Unknown option {} for LSH index", opt_name.as_str()),
                        }
                    }
                    ensure!(
                        false_positive_weight > 0.,
                        "false_positive_weight must be positive"
                    );
                    ensure!(
                        false_negative_weight > 0.,
                        "false_negative_weight must be positive"
                    );
                    ensure!(n_gram > 0, "n_gram must be positive");
                    ensure!(n_perm > 0, "n_perm must be positive");
                    ensure!(
                        target_threshold > 0. && target_threshold < 1.,
                        "target_threshold must be between 0 and 1"
                    );
                    let total_weights = false_positive_weight + false_negative_weight;
                    false_positive_weight /= total_weights;
                    false_negative_weight /= total_weights;

                    if !extract_filter.is_empty() {
                        extractor = format!("if({}, {})", extract_filter, extractor);
                    }

                    let config = MinHashLshConfig {
                        base_relation: SmartString::from(rel.as_str()),
                        index_name: SmartString::from(name.as_str()),
                        extractor,
                        tokenizer,
                        filters,
                        n_gram,
                        n_perm,
                        false_positive_weight: false_positive_weight.into(),
                        false_negative_weight: false_negative_weight.into(),
                        target_threshold: target_threshold.into(),
                    };
                    SysOp::CreateMinHashLshIndex(config)
                }
                Rule::index_drop => {
                    let mut inner = inner.into_inner();
                    let rel = inner.next().unwrap();
                    let name = inner.next().unwrap();
                    SysOp::RemoveIndex(
                        Symbol::new(rel.as_str(), rel.extract_span()),
                        Symbol::new(name.as_str(), name.extract_span()),
                    )
                }
                r => unreachable!("{:?}", r),
            }
        }
        // mnestic fork, graph projection (`docs/specs/graph-projection.md` §3.1)
        Rule::graph_op => {
            let inner = inner.into_inner().next().unwrap();
            match inner.as_rule() {
                Rule::graph_create => {
                    let mut inner = inner.into_inner();
                    let name = inner.next().unwrap();
                    let mut edges = None;
                    let mut nodes = None;
                    for opt_pair in inner {
                        let mut opt_inner = opt_pair.into_inner();
                        let opt_name = opt_inner.next().unwrap();
                        let opt_val = opt_inner.next().unwrap();
                        let slot = match opt_name.as_str() {
                            "edges" => &mut edges,
                            "nodes" => &mut nodes,
                            other => bail!(
                                "unknown option '{other}' for `::graph create`: \
                                 expected 'edges' or 'nodes'"
                            ),
                        };
                        *slot = Some(graph_source_name(opt_val, param_pool)?);
                    }
                    let Some(edges) = edges else {
                        bail!("`::graph create` requires an `edges` relation");
                    };
                    SysOp::CreateGraph {
                        name: SmartString::from(name.as_str()),
                        edges,
                        nodes,
                    }
                }
                Rule::graph_drop => {
                    let name = inner.into_inner().next().unwrap();
                    SysOp::DropGraph(SmartString::from(name.as_str()))
                }
                Rule::graph_list => SysOp::ListGraphs,
                r => unreachable!("{:?}", r),
            }
        }
        Rule::fts_idx_op => {
            let inner = inner.into_inner().next().unwrap();
            match inner.as_rule() {
                Rule::index_create_adv => {
                    let mut inner = inner.into_inner();
                    let rel = inner.next().unwrap();
                    let name = inner.next().unwrap();
                    let mut filters = vec![];
                    let mut tokenizer = TokenizerConfig {
                        name: Default::default(),
                        args: Default::default(),
                    };
                    let mut extractor = "".to_string();
                    let mut extract_filter = "".to_string();
                    for opt_pair in inner {
                        let mut opt_inner = opt_pair.into_inner();
                        let opt_name = opt_inner.next().unwrap();
                        let opt_val = opt_inner.next().unwrap();
                        match opt_name.as_str() {
                            "extractor" => {
                                let mut ex = build_expr(opt_val, param_pool)?;
                                ex.partial_eval()?;
                                extractor = ex.to_string();
                            }
                            "extract_filter" => {
                                let mut ex = build_expr(opt_val, param_pool)?;
                                ex.partial_eval()?;
                                extract_filter = ex.to_string();
                            }
                            "tokenizer" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                match expr {
                                    Expr::UnboundApply { op, args, .. } => {
                                        let mut targs = vec![];
                                        for arg in args.iter() {
                                            let v = arg.clone().eval_to_const()?;
                                            targs.push(v);
                                        }
                                        tokenizer.name = op;
                                        tokenizer.args = targs;
                                    }
                                    Expr::Binding { var, .. } => {
                                        tokenizer.name = var.name;
                                        tokenizer.args = vec![];
                                    }
                                    _ => bail!("Tokenizer must be a symbol or a call for an existing tokenizer"),
                                }
                            }
                            "filters" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                match expr {
                                    Expr::Apply { op, args, .. } => {
                                        if op.name != "OP_LIST" {
                                            bail!("Filters must be a list of filters");
                                        }
                                        for arg in args.iter() {
                                            match arg {
                                                Expr::UnboundApply { op, args, .. } => {
                                                    let mut targs = vec![];
                                                    for arg in args.iter() {
                                                        let v = arg.clone().eval_to_const()?;
                                                        targs.push(v);
                                                    }
                                                    filters.push(TokenizerConfig {
                                                        name: op.clone(),
                                                        args: targs,
                                                    })
                                                }
                                                Expr::Binding { var, .. } => {
                                                    filters.push(TokenizerConfig {
                                                        name: var.name.clone(),
                                                        args: vec![],
                                                    })
                                                }
                                                _ => bail!("Tokenizer must be a symbol or a call for an existing tokenizer"),
                                            }
                                        }
                                    }
                                    _ => bail!("Filters must be a list of filters"),
                                }
                            }
                            _ => bail!("Unknown option {} for FTS index", opt_name.as_str()),
                        }
                    }
                    if !extract_filter.is_empty() {
                        extractor = format!("if({}, {})", extract_filter, extractor);
                    }
                    let config = FtsIndexConfig {
                        base_relation: SmartString::from(rel.as_str()),
                        index_name: SmartString::from(name.as_str()),
                        extractor,
                        tokenizer,
                        filters,
                    };
                    SysOp::CreateFtsIndex(config)
                }
                Rule::index_drop => {
                    let mut inner = inner.into_inner();
                    let rel = inner.next().unwrap();
                    let name = inner.next().unwrap();
                    SysOp::RemoveIndex(
                        Symbol::new(rel.as_str(), rel.extract_span()),
                        Symbol::new(name.as_str(), name.extract_span()),
                    )
                }
                r => unreachable!("{:?}", r),
            }
        }
        Rule::vec_idx_op => {
            let inner = inner.into_inner().next().unwrap();
            match inner.as_rule() {
                Rule::index_create_adv => {
                    let mut inner = inner.into_inner();
                    let rel = inner.next().unwrap();
                    let name = inner.next().unwrap();
                    // options
                    let mut vec_dim = 0;
                    let mut dtype = VecElementType::F32;
                    let mut vec_fields = vec![];
                    let mut distance = HnswDistance::L2;
                    let mut ef_construction = 0;
                    let mut m_neighbours = 0;
                    let mut index_filter = None;
                    let mut extend_candidates = false;
                    let mut keep_pruned_connections = false;

                    for opt_pair in inner {
                        let mut opt_inner = opt_pair.into_inner();
                        let opt_name = opt_inner.next().unwrap();
                        let opt_val = opt_inner.next().unwrap();
                        let opt_val_str = opt_val.as_str();
                        match opt_name.as_str() {
                            "dim" => {
                                let v = build_expr(opt_val, param_pool)?
                                    .eval_to_const()?
                                    .get_int()
                                    .ok_or_else(|| miette!("Invalid vec_dim: {}", opt_val_str))?;
                                ensure!(v > 0, "Invalid vec_dim: {}", v);
                                vec_dim = v as usize;
                            }
                            "ef_construction" | "ef" => {
                                let v = build_expr(opt_val, param_pool)?
                                    .eval_to_const()?
                                    .get_int()
                                    .ok_or_else(|| {
                                        miette!("Invalid ef_construction: {}", opt_val_str)
                                    })?;
                                ensure!(v > 0, "Invalid ef_construction: {}", v);
                                ef_construction = v as usize;
                            }
                            "m_neighbours" | "m" => {
                                let v = build_expr(opt_val, param_pool)?
                                    .eval_to_const()?
                                    .get_int()
                                    .ok_or_else(|| {
                                        miette!("Invalid m_neighbours: {}", opt_val_str)
                                    })?;
                                ensure!(v > 0, "Invalid m_neighbours: {}", v);
                                m_neighbours = v as usize;
                            }
                            "dtype" => {
                                dtype = match opt_val.as_str() {
                                    "F32" | "Float" => VecElementType::F32,
                                    "F64" | "Double" => VecElementType::F64,
                                    _ => {
                                        return Err(miette!("Invalid dtype: {}", opt_val.as_str()))
                                    }
                                }
                            }
                            "fields" => {
                                let fields = build_expr(opt_val, &Default::default())?;
                                vec_fields = fields.to_var_list()?;
                            }
                            "distance" | "dist" => {
                                distance = match opt_val.as_str().trim() {
                                    "L2" => HnswDistance::L2,
                                    "IP" => HnswDistance::InnerProduct,
                                    "Cosine" => HnswDistance::Cosine,
                                    _ => {
                                        return Err(miette!(
                                            "Invalid distance: {}",
                                            opt_val.as_str()
                                        ))
                                    }
                                }
                            }
                            "filter" => {
                                index_filter = Some(opt_val.as_str().to_string());
                            }
                            "extend_candidates" => {
                                extend_candidates = opt_val.as_str().trim() == "true";
                            }
                            "keep_pruned_connections" => {
                                keep_pruned_connections = opt_val.as_str().trim() == "true";
                            }
                            _ => return Err(miette!("Invalid option: {}", opt_name.as_str())),
                        }
                    }
                    if ef_construction == 0 {
                        bail!("ef_construction must be set");
                    }
                    if m_neighbours == 0 {
                        bail!("m_neighbours must be set");
                    }
                    SysOp::CreateVectorIndex(HnswIndexConfig {
                        base_relation: SmartString::from(rel.as_str()),
                        index_name: SmartString::from(name.as_str()),
                        vec_dim,
                        dtype,
                        vec_fields,
                        distance,
                        ef_construction,
                        m_neighbours,
                        index_filter,
                        extend_candidates,
                        keep_pruned_connections,
                    })
                }
                Rule::index_drop => {
                    let mut inner = inner.into_inner();
                    let rel = inner.next().unwrap();
                    let name = inner.next().unwrap();
                    SysOp::RemoveIndex(
                        Symbol::new(rel.as_str(), rel.extract_span()),
                        Symbol::new(name.as_str(), name.extract_span()),
                    )
                }
                r => unreachable!("{:?}", r),
            }
        }
        Rule::index_op => {
            let inner = inner.into_inner().next().unwrap();
            match inner.as_rule() {
                Rule::index_create => {
                    let span = inner.extract_span();
                    let mut inner = inner.into_inner();
                    let rel = inner.next().unwrap();
                    let name = inner.next().unwrap();
                    let cols = inner
                        .map(|p| Symbol::new(p.as_str(), p.extract_span()))
                        .collect_vec();

                    #[derive(Debug, Diagnostic, Error)]
                    #[error("index must have at least one column specified")]
                    #[diagnostic(code(parser::empty_index))]
                    struct EmptyIndex(#[label] SourceSpan);

                    ensure!(!cols.is_empty(), EmptyIndex(span));
                    SysOp::CreateIndex(
                        Symbol::new(rel.as_str(), rel.extract_span()),
                        Symbol::new(name.as_str(), name.extract_span()),
                        cols,
                    )
                }
                Rule::index_drop => {
                    let mut inner = inner.into_inner();
                    let rel = inner.next().unwrap();
                    let name = inner.next().unwrap();
                    SysOp::RemoveIndex(
                        Symbol::new(rel.as_str(), rel.extract_span()),
                        Symbol::new(name.as_str(), name.extract_span()),
                    )
                }
                _ => unreachable!(),
            }
        }
        Rule::list_fixed_rules => SysOp::ListFixedRules,
        r => unreachable!("{:?}", r),
    })
}

/// Evaluate a sysop key argument: a const list of key-lists (mnestic fork).
fn sysop_keys(expr: crate::data::expr::Expr) -> Result<Vec<Vec<DataValue>>> {
    match expr.eval_to_const()? {
        DataValue::List(keys) => keys
            .into_iter()
            .map(|k| match k {
                DataValue::List(parts) => Ok(parts),
                v => Ok(vec![v]),
            })
            .collect(),
        _ => bail!("expected a list of keys, e.g. [[1], [2]]"),
    }
}

/// Read a `::graph create` source relation out of its option expression
/// (mnestic fork). A bare `edges: knows` parses as a binding, a quoted
/// `edges: 'knows'` as a string constant; both name the same relation. Same
/// two shapes `::fts create`'s `tokenizer:` accepts.
fn graph_source_name(
    pair: crate::parse::Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<SmartString<LazyCompact>> {
    let mut expr = build_expr(pair, param_pool)?;
    expr.partial_eval()?;
    if let Expr::Binding { var, .. } = expr {
        return Ok(var.name);
    }
    match expr.eval_to_const()? {
        DataValue::Str(s) => Ok(s),
        _ => bail!("a `::graph create` source must be a relation name, bare or quoted"),
    }
}

/// Parse a bare `pos_int` sysop token (mnestic fork; `::history` limit/offset).
fn sysop_pos_int(pair: &pest::iterators::Pair<'_, Rule>, what: &str) -> Result<usize> {
    pair.as_str()
        .replace('_', "")
        .parse::<usize>()
        .map_err(|_| miette!("{} must be a non-negative integer", what))
}
