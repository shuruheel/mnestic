/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::cmp::Reverse;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use either::{Left, Right};
use itertools::Itertools;
use miette::{bail, ensure, Diagnostic, LabeledSpan, Report, Result};
use pest::Parser;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::aggr::{parse_aggr, Aggregation};
use crate::data::expr::Expr;
use crate::data::functions::{str2vld, MAX_VALIDITY_TS};
use crate::data::program::{
    FixedRuleApply, FixedRuleArg, InputAtom, InputInlineRule, InputInlineRulesOrFixed,
    InputNamedFieldRelationApplyAtom, InputProgram, InputRelationApplyAtom, InputRuleApplyAtom,
    QueryAssertion, QueryOutOptions, RelationOp, ReorderMode, ReturnMutation, SearchInput, SortDir,
    Unification,
};
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::symb::{Symbol, PROG_ENTRY};
use crate::data::value::{DataValue, ValidityTs};
use crate::fixed_rule::utilities::constant::Constant;
use crate::fixed_rule::{FixedRuleHandle, FixedRuleNotFoundError};
use crate::parse::expr::build_expr;
use crate::parse::schema::parse_schema;
use crate::parse::{CozoScriptParser, ExtractSpan, Pair, Pairs, Rule, SourceSpan};
use crate::runtime::relation::InputRelationHandle;
use crate::FixedRule;

#[derive(Error, Diagnostic, Debug)]
#[error("Query option {0} is not constant")]
#[diagnostic(code(parser::option_not_constant))]
struct OptionNotConstantError(&'static str, #[label] SourceSpan, #[related] [Report; 1]);

#[derive(Error, Diagnostic, Debug)]
#[error("Query option {0} requires a non-negative integer")]
#[diagnostic(code(parser::option_not_non_neg))]
struct OptionNotNonNegIntError(&'static str, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("Query option {0} requires a positive integer")]
#[diagnostic(code(parser::option_not_pos))]
struct OptionNotPosIntError(&'static str, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("Query option {0} requires a boolean")]
#[diagnostic(code(parser::option_not_bool))]
struct OptionNotBoolError(&'static str, #[label] SourceSpan);

#[derive(Debug)]
struct MultipleRuleDefinitionError(String, Vec<SourceSpan>);

#[derive(Debug, Error, Diagnostic)]
#[error("Multiple query output assertions defined")]
#[diagnostic(code(parser::multiple_out_assert))]
struct DuplicateQueryAssertion(#[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("Multiple query yields defined")]
#[diagnostic(code(parser::multiple_yields))]
struct DuplicateYield(#[label] SourceSpan);

impl Error for MultipleRuleDefinitionError {}

impl Display for MultipleRuleDefinitionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The rule '{0}' cannot have multiple definitions since it contains non-Horn clauses",
            self.0
        )
    }
}

impl Diagnostic for MultipleRuleDefinitionError {
    fn code<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        Some(Box::new("parser::mult_rule_def"))
    }
    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        Some(Box::new(
            self.1.iter().map(|s| LabeledSpan::new_with_span(None, s)),
        ))
    }
}

fn merge_spans(symbs: &[Symbol]) -> SourceSpan {
    let mut fst = symbs.first().unwrap().span;
    for nxt in symbs.iter().skip(1) {
        fst = fst.merge(nxt.span);
    }
    fst
}

pub(crate) fn parse_query(
    src: Pairs<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<Box<dyn FixedRule>>>,
    custom_aggrs: crate::data::aggr::CustomAggrRegistries<'_>,
    cur_vld: ValidityTs,
) -> Result<InputProgram> {
    let mut progs: BTreeMap<Symbol, InputInlineRulesOrFixed> = Default::default();
    let mut out_opts: QueryOutOptions = Default::default();
    let mut disable_magic_rewrite = false;

    let mut stored_relation = None;
    let mut returning_mutation = ReturnMutation::NotReturning;

    for pair in src {
        match pair.as_rule() {
            Rule::rule => {
                let (name, rule) = parse_rule(pair, param_pool, custom_aggrs, cur_vld)?;

                match progs.entry(name) {
                    Entry::Vacant(e) => {
                        e.insert(InputInlineRulesOrFixed::Rules { rules: vec![rule] });
                    }
                    Entry::Occupied(mut e) => {
                        let key = e.key().to_string();
                        match e.get_mut() {
                            InputInlineRulesOrFixed::Rules { rules: rs } => {
                                #[derive(Debug, Error, Diagnostic)]
                                #[error("Rule {0} has multiple definitions with conflicting heads")]
                                #[diagnostic(code(parser::head_aggr_mismatch))]
                                #[diagnostic(help("The arity of each rule head must match. In addition, any aggregation \
                            applied must be the same."))]
                                struct RuleHeadMismatch(
                                    String,
                                    #[label] SourceSpan,
                                    #[label] SourceSpan,
                                );
                                let prev = rs.first().unwrap();
                                ensure!(prev.aggr == rule.aggr, {
                                    RuleHeadMismatch(
                                        key,
                                        merge_spans(&prev.head),
                                        merge_spans(&rule.head),
                                    )
                                });

                                rs.push(rule);
                            }
                            InputInlineRulesOrFixed::Fixed { fixed } => {
                                let fixed_span = fixed.span;
                                bail!(MultipleRuleDefinitionError(
                                    e.key().name.to_string(),
                                    vec![rule.span, fixed_span]
                                ))
                            }
                        }
                    }
                }
            }
            Rule::fixed_rule => {
                let rule_span = pair.extract_span();
                let (name, apply) =
                    parse_fixed_rule(pair, param_pool, fixed_rules, custom_aggrs, cur_vld)?;

                match progs.entry(name) {
                    Entry::Vacant(e) => {
                        e.insert(InputInlineRulesOrFixed::Fixed { fixed: apply });
                    }
                    Entry::Occupied(e) => {
                        let found_name = e.key().name.to_string();
                        let mut found_span = match e.get() {
                            InputInlineRulesOrFixed::Rules { rules } => {
                                rules.iter().map(|r| r.span).collect_vec()
                            }
                            InputInlineRulesOrFixed::Fixed { fixed } => vec![fixed.span],
                        };
                        found_span.push(rule_span);
                        bail!(MultipleRuleDefinitionError(found_name, found_span));
                    }
                }
            }
            Rule::const_rule => {
                let span = pair.extract_span();
                let mut src = pair.into_inner();
                let (name, mut head, aggr) =
                    parse_rule_head(src.next().unwrap(), param_pool, custom_aggrs)?;

                if let Some(found) = progs.get(&name) {
                    let mut found_span = match found {
                        InputInlineRulesOrFixed::Rules { rules } => {
                            rules.iter().map(|r| r.span).collect_vec()
                        }
                        InputInlineRulesOrFixed::Fixed { fixed } => {
                            vec![fixed.span]
                        }
                    };
                    found_span.push(span);
                    bail!(MultipleRuleDefinitionError(
                        name.name.to_string(),
                        found_span
                    ));
                }

                #[derive(Debug, Error, Diagnostic)]
                #[error("Constant rules cannot have aggregation application")]
                #[diagnostic(code(parser::aggr_in_const_rule))]
                struct AggrInConstRuleError(#[label] SourceSpan);

                for (a, v) in aggr.iter().zip(head.iter()) {
                    ensure!(a.is_none(), AggrInConstRuleError(v.span));
                }
                let data_part = src.next().unwrap();
                let data_part_str = data_part.as_str();
                let data = build_expr(data_part.clone(), param_pool)?;
                let mut options = BTreeMap::new();
                options.insert(SmartString::from("data"), data);
                let handle = FixedRuleHandle {
                    name: Symbol::new("Constant", span),
                };
                let fixed_impl = Box::new(Constant);
                fixed_impl.init_options(&mut options, span)?;
                let arity = fixed_impl.arity(&options, &head, span)?;

                ensure!(arity != 0, EmptyRowForConstRule(span));
                ensure!(
                    head.is_empty() || arity == head.len(),
                    FixedRuleHeadArityMismatch(arity, head.len(), span)
                );
                if head.is_empty() && name.is_prog_entry() {
                    if let Ok(mut datalist) =
                        CozoScriptParser::parse(Rule::param_list, data_part_str)
                    {
                        for s in datalist.next().unwrap().into_inner() {
                            if s.as_rule() == Rule::param {
                                head.push(Symbol::new(
                                    s.as_str().strip_prefix('$').unwrap(),
                                    Default::default(),
                                ));
                            }
                        }
                    }
                }
                progs.insert(
                    name,
                    InputInlineRulesOrFixed::Fixed {
                        fixed: FixedRuleApply {
                            fixed_handle: handle,
                            rule_args: vec![],
                            options: Arc::new(options),
                            head,
                            arity,
                            span,
                            fixed_impl: Arc::new(fixed_impl),
                        },
                    },
                );
            }
            Rule::as_of_option => {
                // mnestic fork, bitemporality §4: default tt for every
                // tt-stamped relation atom in this query lacking an explicit
                // selector. Parsed with the tt token rules (NOW/END = current
                // belief; numeric µs; ISO-8601).
                let pair = pair.into_inner().next().unwrap();
                let expr = build_expr(pair, param_pool)?;
                out_opts.as_of = Some(expr2tt_spec(expr)?);
            }
            Rule::timeout_option => {
                let pair = pair.into_inner().next().unwrap();
                let span = pair.extract_span();
                let timeout = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("timeout", span, [err]))?
                    .get_float()
                    .ok_or(OptionNotNonNegIntError("timeout", span))?;
                if timeout > 0. {
                    out_opts.timeout = Some(timeout);
                } else {
                    out_opts.timeout = None;
                }
            }
            Rule::sleep_option => {
                #[cfg(target_arch = "wasm32")]
                bail!(":sleep is not supported under WASM");

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let pair = pair.into_inner().next().unwrap();
                    let span = pair.extract_span();
                    let sleep = build_expr(pair, param_pool)?
                        .eval_to_const()
                        .map_err(|err| OptionNotConstantError("sleep", span, [err]))?
                        .get_float()
                        .ok_or(OptionNotNonNegIntError("sleep", span))?;
                    ensure!(sleep > 0., OptionNotPosIntError("sleep", span));
                    out_opts.sleep = Some(sleep);
                }
            }
            Rule::limit_option => {
                let pair = pair.into_inner().next().unwrap();
                let span = pair.extract_span();
                let limit = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("limit", span, [err]))?
                    .get_non_neg_int()
                    .ok_or(OptionNotNonNegIntError("limit", span))?;
                out_opts.limit = Some(limit as usize);
            }
            Rule::offset_option => {
                let pair = pair.into_inner().next().unwrap();
                let span = pair.extract_span();
                let offset = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("offset", span, [err]))?
                    .get_non_neg_int()
                    .ok_or(OptionNotNonNegIntError("offset", span))?;
                out_opts.offset = Some(offset as usize);
            }
            Rule::sort_option => {
                for part in pair.into_inner() {
                    let mut var = "";
                    let mut dir = SortDir::Asc;
                    let mut span = part.extract_span();
                    for a in part.into_inner() {
                        match a.as_rule() {
                            Rule::out_arg => {
                                var = a.as_str();
                                span = a.extract_span();
                            }
                            Rule::sort_asc => dir = SortDir::Asc,
                            Rule::sort_desc => dir = SortDir::Dsc,
                            _ => unreachable!(),
                        }
                    }
                    out_opts.sorters.push((Symbol::new(var, span), dir));
                }
            }
            Rule::returning_option => {
                returning_mutation = ReturnMutation::Returning;
            }
            Rule::relation_option => {
                let span = pair.extract_span();
                let mut args = pair.into_inner();
                let op = match args.next().unwrap().as_rule() {
                    Rule::relation_create => RelationOp::Create,
                    Rule::relation_replace => RelationOp::Replace,
                    Rule::relation_reconcile => RelationOp::Reconcile,
                    Rule::relation_put => RelationOp::Put,
                    Rule::relation_insert => RelationOp::Insert,
                    Rule::relation_update => RelationOp::Update,
                    Rule::relation_rm => RelationOp::Rm,
                    Rule::relation_delete => RelationOp::Delete,
                    Rule::relation_ensure => RelationOp::Ensure,
                    Rule::relation_ensure_not => RelationOp::EnsureNot,
                    _ => unreachable!(),
                };

                let name_p = args.next().unwrap();
                let name = Symbol::new(name_p.as_str(), name_p.extract_span());
                match args.next() {
                    None => stored_relation = Some(Left((name, span, op))),
                    Some(schema_p) => {
                        let (mut metadata, mut key_bindings, mut dep_bindings) =
                            parse_schema(schema_p)?;
                        if !matches!(op, RelationOp::Create | RelationOp::Replace) {
                            key_bindings.extend(dep_bindings);
                            dep_bindings = vec![];
                            metadata.keys.extend(metadata.non_keys);
                            metadata.non_keys = vec![];
                        }
                        stored_relation = Some(Right((
                            InputRelationHandle {
                                name,
                                metadata,
                                key_bindings,
                                dep_bindings,
                                span,
                            },
                            op,
                        )))
                    }
                }
            }
            Rule::assert_none_option => {
                ensure!(
                    out_opts.assertion.is_none(),
                    DuplicateQueryAssertion(pair.extract_span())
                );
                out_opts.assertion = Some(QueryAssertion::AssertNone(pair.extract_span()))
            }
            Rule::assert_some_option => {
                ensure!(
                    out_opts.assertion.is_none(),
                    DuplicateQueryAssertion(pair.extract_span())
                );
                out_opts.assertion = Some(QueryAssertion::AssertSome(pair.extract_span()))
            }
            Rule::reorder_option => {
                // mnestic (join-reorder, 0.10.5). `:reorder written` opts out of
                // the default greedy join reorder; `:reorder greedy` forces it on.
                #[derive(Debug, Error, Diagnostic)]
                #[error("unknown :reorder mode '{0}', expected 'greedy' or 'written'")]
                #[diagnostic(code(parser::bad_reorder_mode))]
                struct BadReorderMode(String, #[label] SourceSpan);

                let inner = pair.into_inner().next().unwrap();
                let span = inner.extract_span();
                match inner.as_str() {
                    "greedy" => out_opts.reorder = ReorderMode::Greedy,
                    "written" => out_opts.reorder = ReorderMode::Written,
                    other => bail!(BadReorderMode(other.to_string(), span)),
                }
            }
            Rule::disable_magic_rewrite_option => {
                let pair = pair.into_inner().next().unwrap();
                let span = pair.extract_span();
                let val = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("disable_magic_rewrite", span, [err]))?
                    .get_bool()
                    .ok_or(OptionNotBoolError("disable_magic_rewrite", span))?;
                disable_magic_rewrite = val;
            }
            Rule::EOI => break,
            r => unreachable!("{:?}", r),
        }
    }

    let mut prog = InputProgram {
        prog: progs,
        out_opts,
        disable_magic_rewrite,
    };

    if prog.prog.is_empty() {
        if let Some((
            InputRelationHandle {
                key_bindings,
                dep_bindings,
                ..
            },
            RelationOp::Create,
            _,
        )) = &prog.out_opts.store_relation
        {
            let mut bindings = key_bindings.clone();
            bindings.extend_from_slice(dep_bindings);
            make_empty_const_rule(&mut prog, &bindings);
        }
    }

    // let head_arity = prog.get_entry_arity()?;

    match stored_relation {
        None => {}
        Some(Left((name, span, op))) => {
            let head = prog.get_entry_out_head()?;
            for symb in &head {
                symb.ensure_valid_field()?;
            }

            let metadata = StoredRelationMetadata {
                keys: head
                    .iter()
                    .map(|s| ColumnDef {
                        name: s.name.clone(),
                        typing: NullableColType {
                            coltype: ColType::Any,
                            nullable: true,
                        },
                        default_gen: None,
                    })
                    .collect(),
                non_keys: vec![],
            };

            let handle = InputRelationHandle {
                name,
                metadata,
                key_bindings: head,
                dep_bindings: vec![],
                span,
            };
            prog.out_opts.store_relation = Some((handle, op, returning_mutation))
        }
        Some(Right((h, o))) => prog.out_opts.store_relation = Some((h, o, returning_mutation)),
    }

    if prog.prog.is_empty() {
        if let Some((handle, RelationOp::Create, _)) = &prog.out_opts.store_relation {
            let mut bindings = handle.dep_bindings.clone();
            bindings.extend_from_slice(&handle.key_bindings);
            make_empty_const_rule(&mut prog, &bindings);
        }
    }

    if !prog.out_opts.sorters.is_empty() {
        #[derive(Debug, Error, Diagnostic)]
        #[error("Sort key '{0}' not found")]
        #[diagnostic(code(parser::sort_key_not_found))]
        struct SortKeyNotFound(String, #[label] SourceSpan);

        let head_args = prog.get_entry_out_head()?;

        for (sorter, _) in &prog.out_opts.sorters {
            ensure!(
                head_args.contains(sorter),
                SortKeyNotFound(sorter.to_string(), sorter.span)
            )
        }
    }

    #[derive(Debug, Error, Diagnostic)]
    #[error("Input relation '{0}' has no keys")]
    #[diagnostic(code(parser::relation_has_no_keys))]
    struct RelationHasNoKeys(String, #[label] SourceSpan);

    let empty_mutation_head = match &prog.out_opts.store_relation {
        Some((handle, _, _)) if handle.key_bindings.is_empty() => {
            if handle.dep_bindings.is_empty() {
                true
            } else {
                bail!(RelationHasNoKeys(handle.name.to_string(), handle.span));
            }
        }
        _ => false,
    };

    if empty_mutation_head {
        let head_args = prog.get_entry_out_head()?;
        if let Some((handle, _, _)) = &mut prog.out_opts.store_relation {
            if head_args.is_empty() {
                bail!(RelationHasNoKeys(handle.name.to_string(), handle.span));
            }
            handle.key_bindings = head_args.clone();
            handle.metadata.keys = head_args
                .iter()
                .map(|s| ColumnDef {
                    name: s.name.clone(),
                    typing: NullableColType {
                        coltype: ColType::Any,
                        nullable: true,
                    },
                    default_gen: None,
                })
                .collect();
        } else {
            unreachable!()
        }
    }

    Ok(prog)
}

fn parse_rule(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    custom_aggrs: crate::data::aggr::CustomAggrRegistries<'_>,
    cur_vld: ValidityTs,
) -> Result<(Symbol, InputInlineRule)> {
    let span = src.extract_span();
    let mut src = src.into_inner();
    let head = src.next().unwrap();
    let head_span = head.extract_span();
    let (name, head, aggr) = parse_rule_head(head, param_pool, custom_aggrs)?;

    #[derive(Debug, Error, Diagnostic)]
    #[error("Horn-clause rule cannot have empty rule head")]
    #[diagnostic(code(parser::empty_horn_rule_head))]
    struct EmptyRuleHead(#[label] SourceSpan);

    ensure!(!head.is_empty(), EmptyRuleHead(head_span));
    let body = src.next().unwrap();
    let mut body_clauses = vec![];
    let mut ignored_counter = 0;
    for atom_src in body.into_inner() {
        body_clauses.push(parse_disjunction(
            atom_src,
            param_pool,
            cur_vld,
            &mut ignored_counter,
        )?)
    }

    Ok((
        name,
        InputInlineRule {
            head,
            aggr,
            body: body_clauses,
            span,
        },
    ))
}

fn parse_disjunction(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
    ignored_counter: &mut u32,
) -> Result<InputAtom> {
    let span = pair.extract_span();
    let res: Vec<_> = pair
        .into_inner()
        .filter_map(|v| match v.as_rule() {
            Rule::or_op => None,
            _ => Some(parse_atom(v, param_pool, cur_vld, ignored_counter)),
        })
        .try_collect()?;
    Ok(if res.len() == 1 {
        res.into_iter().next().unwrap()
    } else {
        InputAtom::Disjunction { inner: res, span }
    })
}

fn parse_atom(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
    ignored_counter: &mut u32,
) -> Result<InputAtom> {
    Ok(match src.as_rule() {
        Rule::rule_body => {
            let span = src.extract_span();
            let grouped: Vec<_> = src
                .into_inner()
                .map(|v| parse_disjunction(v, param_pool, cur_vld, ignored_counter))
                .try_collect()?;
            InputAtom::Conjunction {
                inner: grouped,
                span,
            }
        }
        Rule::disjunction => parse_disjunction(src, param_pool, cur_vld, ignored_counter)?,
        Rule::negation => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            src.next().unwrap();
            let inner = parse_atom(src.next().unwrap(), param_pool, cur_vld, ignored_counter)?;
            InputAtom::Negation {
                inner: inner.into(),
                span,
            }
        }
        Rule::expr => {
            let expr = build_expr(src, param_pool)?;
            InputAtom::Predicate { inner: expr }
        }
        Rule::unify => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let var = src.next().unwrap();
            let mut symb = Symbol::new(var.as_str(), var.extract_span());
            if symb.is_ignored_symbol() {
                symb.name = format!("*^*{}", *ignored_counter).into();
                *ignored_counter += 1;
            }
            let expr = build_expr(src.next().unwrap(), param_pool)?;
            InputAtom::Unification {
                inner: Unification {
                    binding: symb,
                    expr,
                    one_many_unif: false,
                    span,
                },
            }
        }
        Rule::unify_multi => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let var = src.next().unwrap();
            let mut symb = Symbol::new(var.as_str(), var.extract_span());
            if symb.is_ignored_symbol() {
                symb.name = format!("*^*{}", *ignored_counter).into();
                *ignored_counter += 1;
            }
            src.next().unwrap();
            let expr = build_expr(src.next().unwrap(), param_pool)?;
            InputAtom::Unification {
                inner: Unification {
                    binding: symb,
                    expr,
                    one_many_unif: true,
                    span,
                },
            }
        }
        Rule::rule_apply => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let name = src.next().unwrap();
            let args: Vec<_> = src
                .next()
                .unwrap()
                .into_inner()
                .map(|v| build_expr(v, param_pool))
                .try_collect()?;
            InputAtom::Rule {
                inner: InputRuleApplyAtom {
                    name: Symbol::new(name.as_str(), name.extract_span()),
                    args,
                    span,
                },
            }
        }
        Rule::relation_apply => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let name = src.next().unwrap();
            let args: Vec<_> = src
                .next()
                .unwrap()
                .into_inner()
                .map(|v| build_expr(v, param_pool))
                .try_collect()?;
            let (valid_at, tx_valid_at) = match src.next() {
                None => (None, None),
                Some(vld_clause) => parse_temporal_clause(vld_clause, param_pool, cur_vld)?,
            };
            InputAtom::Relation {
                inner: InputRelationApplyAtom {
                    name: Symbol::new(&name.as_str()[1..], name.extract_span()),
                    args,
                    valid_at,
                    tx_valid_at,
                    span,
                },
            }
        }
        Rule::search_apply => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let name_p = src.next().unwrap();
            let name_segs = name_p.as_str().split(':').collect_vec();

            #[derive(Debug, Error, Diagnostic)]
            #[error("Search head must be of the form `relation_name:index_name`")]
            #[diagnostic(code(parser::invalid_search_head))]
            struct InvalidSearchHead(#[label] SourceSpan);

            ensure!(
                name_segs.len() == 2,
                InvalidSearchHead(name_p.extract_span())
            );
            let relation = Symbol::new(name_segs[0], name_p.extract_span());
            let index = Symbol::new(name_segs[1], name_p.extract_span());
            let bindings: BTreeMap<SmartString<LazyCompact>, Expr> = src
                .next()
                .unwrap()
                .into_inner()
                .map(|arg| extract_named_apply_arg(arg, param_pool))
                .try_collect()?;
            let parameters: BTreeMap<SmartString<LazyCompact>, Expr> = src
                .map(|arg| extract_named_apply_arg(arg, param_pool))
                .try_collect()?;

            let opts = SearchInput {
                relation,
                index,
                bindings,
                span,
                parameters,
            };

            InputAtom::Search { inner: opts }
        }
        Rule::relation_named_apply => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let name_p = src.next().unwrap();
            let name = Symbol::new(&name_p.as_str()[1..], name_p.extract_span());
            let args = src
                .next()
                .unwrap()
                .into_inner()
                .map(|arg| extract_named_apply_arg(arg, param_pool))
                .try_collect()?;
            let (valid_at, tx_valid_at) = match src.next() {
                None => (None, None),
                Some(vld_clause) => parse_temporal_clause(vld_clause, param_pool, cur_vld)?,
            };
            InputAtom::NamedFieldRelation {
                inner: InputNamedFieldRelationApplyAtom {
                    name,
                    args,
                    span,
                    valid_at,
                    tx_valid_at,
                },
            }
        }
        r => unreachable!("{:?}", r),
    })
}

fn extract_named_apply_arg(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<(SmartString<LazyCompact>, Expr)> {
    let mut inner = pair.into_inner();
    let name_p = inner.next().unwrap();
    let name = SmartString::from(name_p.as_str());
    let arg = match inner.next() {
        Some(a) => build_expr(a, param_pool)?,
        None => Expr::Binding {
            var: Symbol::new(name.clone(), name_p.extract_span()),
            tuple_pos: None,
        },
    };
    Ok((name, arg))
}

fn parse_rule_head(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    custom_aggrs: crate::data::aggr::CustomAggrRegistries<'_>,
) -> Result<(
    Symbol,
    Vec<Symbol>,
    Vec<Option<(Aggregation, Vec<DataValue>)>>,
)> {
    let mut src = src.into_inner();
    let name = src.next().unwrap();
    let mut args = vec![];
    let mut aggrs = vec![];
    for p in src {
        let (arg, aggr) = parse_rule_head_arg(p, param_pool, custom_aggrs)?;
        args.push(arg);
        aggrs.push(aggr);
    }
    Ok((Symbol::new(name.as_str(), name.extract_span()), args, aggrs))
}

#[derive(Error, Diagnostic, Debug)]
#[diagnostic(code(parser::aggr_not_found))]
#[error("Aggregation '{0}' not found")]
struct AggrNotFound(String, #[label] SourceSpan);

fn parse_rule_head_arg(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    custom_aggrs: crate::data::aggr::CustomAggrRegistries<'_>,
) -> Result<(Symbol, Option<(Aggregation, Vec<DataValue>)>)> {
    let src = src.into_inner().next().unwrap();
    Ok(match src.as_rule() {
        Rule::var => (Symbol::new(src.as_str(), src.extract_span()), None),
        Rule::aggr_arg => {
            let mut inner = src.into_inner();
            let aggr_p = inner.next().unwrap();
            let aggr_name = aggr_p.as_str();
            let var = inner.next().unwrap();
            let args: Vec<_> = inner
                .map(|v| -> Result<DataValue> { build_expr(v, param_pool)?.eval_to_const() })
                .try_collect()?;
            let aggr = match parse_aggr(aggr_name) {
                Some(builtin) => {
                    // mnestic fork — the built-in skyline aggregates
                    // (`pareto_min`/`pareto_max`) resolve as reserved builtins
                    // (`is_bounded_meet`), but their native dominance must be
                    // attached here: an `Arc` cannot live in the `const` static
                    // `parse_aggr` returns. With `bounded_dominance` populated
                    // they route through the `DominanceMeetStore` exactly like a
                    // host-registered dominance — no store/eval/stratify change.
                    match crate::data::aggr::builtin_skyline_dominance(builtin.name) {
                        Some(reg) => {
                            ensure!(
                                args.is_empty(),
                                "the skyline aggregate '{}' takes no arguments: min-vs-max is the aggregate name, and mixed objectives are encoded by negating the maximised components",
                                builtin.name
                            );
                            let mut owned = builtin.clone();
                            owned.bounded_dominance = Some(reg);
                            owned
                        }
                        None => builtin.clone(),
                    }
                }
                None => match custom_aggrs.meet.get(aggr_name) {
                    // A user-registered aggregate (mnestic fork, R0b): an
                    // owned Aggregation carrying the ⊕ factory. The stratifier
                    // and aggr_kind read only `is_meet`, so a registered meet
                    // rides the recursion guard exactly like a builtin.
                    Some(reg) => Aggregation {
                        name: crate::data::aggr::intern_aggr_name(aggr_name),
                        is_meet: reg.is_meet,
                        is_bounded_meet: false,
                        meet_op: None,
                        normal_op: None,
                        meet_factory: Some(reg.factory.clone()),
                        bounded_dominance: None,
                    },
                    // A registered dominance bounded-meet (mnestic fork,
                    // spec docs/specs/antichain-bounded-meet.md): rides
                    // AggrKind::BoundedMeet exactly like min_cost_k. The cap
                    // lives in the registration, so call-site args are
                    // rejected loudly rather than silently ignored.
                    None => match custom_aggrs.bounded.get(aggr_name) {
                        Some(reg) => {
                            ensure!(
                                args.is_empty(),
                                "registered bounded-meet aggregate '{}' takes no arguments: its cap is set at registration",
                                aggr_name
                            );
                            Aggregation {
                                name: crate::data::aggr::intern_aggr_name(aggr_name),
                                is_meet: false,
                                is_bounded_meet: true,
                                meet_op: None,
                                normal_op: None,
                                meet_factory: None,
                                bounded_dominance: Some(reg.clone()),
                            }
                        }
                        None => {
                            return Err(
                                AggrNotFound(aggr_name.to_string(), aggr_p.extract_span()).into()
                            )
                        }
                    },
                },
            };
            (
                Symbol::new(var.as_str(), var.extract_span()),
                Some((aggr, args)),
            )
        }
        _ => unreachable!(),
    })
}

#[derive(Debug, Error, Diagnostic)]
#[error("bad specification of validity")]
#[diagnostic(code(parser::bad_validity_spec))]
struct BadValiditySpecification(#[label] SourceSpan);

/// Which temporal axis a rejected timestamp was destined for. Only used to make
/// [`FloatValiditySpecification`] name the right one.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TemporalAxis {
    Valid,
    Transaction,
}

impl Display for TemporalAxis {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TemporalAxis::Valid => write!(f, "valid-time"),
            TemporalAxis::Transaction => write!(f, "transaction-time"),
        }
    }
}

/// A float in a validity/tx-time selector is *always* a units bug, so it is worth
/// its own diagnostic rather than folding into [`BadValiditySpecification`]: the
/// float is nearly always `now()`/`parse_timestamp()` seconds, and the whole point
/// of rejecting it is to say what to write instead.
#[derive(Debug, Error, Diagnostic)]
#[error("{1} timestamp must be an integer, got the float {2}")]
#[diagnostic(code(parser::float_validity_spec))]
#[diagnostic(help(
    "{1} timestamps are integer MICROSECONDS since the Unix epoch. `now()` and \
     `parse_timestamp()` return float SECONDS, so a float here would be read about \
     1,000,000x too small — i.e. 1970, before your data exists.\n\
     If the value is in seconds:      to_int(<expr> * 1000000)\n\
     If it is already microseconds:   to_int(<expr>)\n\
     Note that `round()` returns a float and will NOT convert it.\n\
     Non-numeric forms also work: '2024-06-01', '2024-06-01T12:00:00Z', 'NOW', 'END'."
))]
struct FloatValiditySpecification(
    #[label("this must be an integer, not a float")] SourceSpan,
    TemporalAxis,
    f64,
);

fn parse_fixed_rule(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<Box<dyn FixedRule>>>,
    custom_aggrs: crate::data::aggr::CustomAggrRegistries<'_>,
    cur_vld: ValidityTs,
) -> Result<(Symbol, FixedRuleApply)> {
    let mut src = src.into_inner();
    let (out_symbol, head, aggr) = parse_rule_head(src.next().unwrap(), param_pool, custom_aggrs)?;

    #[derive(Debug, Error, Diagnostic)]
    #[error("fixed rule cannot be combined with aggregation")]
    #[diagnostic(code(parser::fixed_aggr_conflict))]
    struct AggrInfixedError(#[label] SourceSpan);

    #[derive(Debug, Error, Diagnostic)]
    #[error("fixed rule cannot have duplicate bindings")]
    #[diagnostic(code(parser::duplicate_bindings_for_fixed_rule))]
    struct DuplicateBindingError(#[label] SourceSpan);

    for (a, v) in aggr.iter().zip(head.iter()) {
        ensure!(a.is_none(), AggrInfixedError(v.span))
    }

    let mut seen_bindings = BTreeSet::new();
    let mut binding_gen_id = 0;

    let name_pair = src.next().unwrap();
    let fixed_name = &name_pair.as_str();
    let mut rule_args: Vec<FixedRuleArg> = vec![];
    let mut options: BTreeMap<SmartString<LazyCompact>, Expr> = Default::default();
    let args_list = src.next().unwrap();
    let args_list_span = args_list.extract_span();

    for nxt in args_list.into_inner() {
        match nxt.as_rule() {
            Rule::fixed_rel => {
                let inner = nxt.into_inner().next().unwrap();
                let span = inner.extract_span();
                match inner.as_rule() {
                    Rule::fixed_rule_rel => {
                        let mut els = inner.into_inner();
                        let name = els.next().unwrap();
                        let mut bindings = Vec::with_capacity(els.size_hint().1.unwrap_or(4));
                        for v in els {
                            let s = v.as_str();
                            if s == "_" {
                                let symb =
                                    Symbol::new(format!("*_*{binding_gen_id}"), v.extract_span());
                                binding_gen_id += 1;
                                bindings.push(symb);
                            } else {
                                if !seen_bindings.insert(s) {
                                    bail!(DuplicateBindingError(v.extract_span()))
                                }
                                let symb = Symbol::new(s, v.extract_span());
                                bindings.push(symb);
                            }
                        }
                        rule_args.push(FixedRuleArg::InMem {
                            name: Symbol::new(name.as_str(), name.extract_span()),
                            bindings,
                            span,
                        })
                    }
                    Rule::fixed_relation_rel => {
                        let mut els = inner.into_inner();
                        let name = els.next().unwrap();
                        let mut bindings = vec![];
                        let mut valid_at = None;
                        let mut tx_valid_at = None;
                        for v in els {
                            match v.as_rule() {
                                Rule::var => {
                                    let s = v.as_str();
                                    if s == "_" {
                                        let symb = Symbol::new(
                                            format!("*_*{binding_gen_id}"),
                                            v.extract_span(),
                                        );
                                        binding_gen_id += 1;
                                        bindings.push(symb);
                                    } else {
                                        if !seen_bindings.insert(s) {
                                            bail!(DuplicateBindingError(v.extract_span()))
                                        }
                                        bindings.push(Symbol::new(v.as_str(), v.extract_span()))
                                    }
                                }
                                Rule::validity_clause => {
                                    let (vt, tt) = parse_temporal_clause(v, param_pool, cur_vld)?;
                                    valid_at = vt;
                                    tx_valid_at = tt;
                                }
                                _ => unreachable!(),
                            }
                        }
                        rule_args.push(FixedRuleArg::Stored {
                            name: Symbol::new(
                                name.as_str().strip_prefix('*').unwrap(),
                                name.extract_span(),
                            ),
                            bindings,
                            valid_at,
                            tx_valid_at,
                            span,
                        })
                    }
                    Rule::fixed_named_relation_rel => {
                        let mut els = inner.into_inner();
                        let name = els.next().unwrap();
                        let mut bindings = BTreeMap::new();
                        let mut valid_at = None;
                        let mut tx_valid_at = None;
                        for p in els {
                            match p.as_rule() {
                                Rule::fixed_named_relation_arg_pair => {
                                    let mut vs = p.into_inner();
                                    let kp = vs.next().unwrap();
                                    let k = SmartString::from(kp.as_str());
                                    let v = match vs.next() {
                                        Some(vp) => {
                                            if !seen_bindings.insert(vp.as_str()) {
                                                bail!(DuplicateBindingError(vp.extract_span()))
                                            }
                                            Symbol::new(vp.as_str(), vp.extract_span())
                                        }
                                        None => {
                                            if !seen_bindings.insert(kp.as_str()) {
                                                bail!(DuplicateBindingError(kp.extract_span()))
                                            }
                                            Symbol::new(k.clone(), kp.extract_span())
                                        }
                                    };
                                    bindings.insert(k, v);
                                }
                                Rule::validity_clause => {
                                    let (vt, tt) = parse_temporal_clause(p, param_pool, cur_vld)?;
                                    valid_at = vt;
                                    tx_valid_at = tt;
                                }
                                _ => unreachable!(),
                            }
                        }

                        rule_args.push(FixedRuleArg::NamedStored {
                            // mnestic fork: this stripped ':' — but the grammar
                            // (`relation_ident = @{"*" ~ …}`) always prefixes
                            // '*', so the unwrap ALWAYS panicked and the named
                            // `*rel{col}` fixed-rule input form was unusable.
                            // Inherited upstream bug, present at fork-base.
                            name: Symbol::new(
                                name.as_str().strip_prefix('*').unwrap(),
                                name.extract_span(),
                            ),
                            bindings,
                            valid_at,
                            tx_valid_at,
                            span,
                        })
                    }
                    _ => unreachable!(),
                }
            }
            Rule::fixed_opt_pair => {
                let mut inner = nxt.into_inner();
                let name = inner.next().unwrap().as_str();
                let val = inner.next().unwrap();
                let val = build_expr(val, param_pool)?;
                options.insert(SmartString::from(name), val);
            }
            _ => unreachable!(),
        }
    }

    let fixed = FixedRuleHandle::new(fixed_name, name_pair.extract_span());

    let fixed_impl = fixed_rules
        .get(&fixed.name as &str)
        .ok_or_else(|| FixedRuleNotFoundError(fixed.name.to_string(), name_pair.extract_span()))?;
    fixed_impl.init_options(&mut options, args_list_span)?;

    // Unknown fixed-rule options are silently ignored engine-wide, so `graph:`
    // on a rule that never reads it would quietly fall back to a full rebuild
    // — the opposite of what the user asked for (mnestic fork,
    // `docs/specs/graph-projection.md` §3.5.4).
    #[derive(Debug, Error, Diagnostic)]
    #[error("'{0}' cannot take its edges from a graph projection")]
    #[diagnostic(code(parser::fixed_rule_no_projection))]
    #[diagnostic(help(
        "this rule does not read a `graph:` option, and unread fixed-rule options are ignored \
         engine-wide — the call would silently run without the projection. Drop the option. \
         The lazy graph algorithms (BFS, DFS, ShortestPathBFS, RandomWalk, ShortestPathAStar, \
         DegreeCentrality) are excluded by design — they evaluate per-tuple expressions a \
         cached adjacency does not carry — and take their edge relation positionally"
    ))]
    struct FixedRuleNoProjectionError(String, #[label] SourceSpan);

    if let Some(opt) = options.get("graph") {
        ensure!(
            fixed_impl.supports_projection(),
            FixedRuleNoProjectionError(fixed.name.to_string(), opt.span())
        );
    }

    let arity = fixed_impl.arity(&options, &head, name_pair.extract_span())?;

    ensure!(
        head.is_empty() || arity == head.len(),
        FixedRuleHeadArityMismatch(arity, head.len(), args_list_span)
    );

    Ok((
        out_symbol,
        FixedRuleApply {
            fixed_handle: fixed,
            rule_args,
            options: Arc::new(options),
            head,
            arity,
            span: args_list_span,
            fixed_impl: fixed_impl.clone(),
        },
    ))
}

#[derive(Debug, Error, Diagnostic)]
#[error("Fixed rule head arity mismatch")]
#[diagnostic(code(parser::fixed_rule_head_arity_mismatch))]
#[diagnostic(help("Expected arity: {0}, number of arguments given: {1}"))]
struct FixedRuleHeadArityMismatch(usize, usize, #[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("Encountered empty row for constant rule")]
#[diagnostic(code(parser::const_rule_empty_row))]
struct EmptyRowForConstRule(#[label] SourceSpan);

fn make_empty_const_rule(prog: &mut InputProgram, bindings: &[Symbol]) {
    let entry_symbol = Symbol::new(PROG_ENTRY, Default::default());
    let mut options = BTreeMap::new();
    options.insert(
        SmartString::from("data"),
        Expr::Const {
            val: DataValue::List(vec![]),
            span: Default::default(),
        },
    );
    prog.prog.insert(
        entry_symbol,
        InputInlineRulesOrFixed::Fixed {
            fixed: FixedRuleApply {
                fixed_handle: FixedRuleHandle {
                    name: Symbol::new("Constant", Default::default()),
                },
                rule_args: vec![],
                options: Arc::new(options),
                head: bindings.to_vec(),
                arity: bindings.len(),
                span: Default::default(),
                fixed_impl: Arc::new(Box::new(Constant)),
            },
        },
    );
}

/// Parse a transaction-time selector expr (mnestic fork, bitemporality §4).
/// "NOW" and "END" both resolve to end-of-tt-time (`MAX_VALIDITY_TS`):
/// "current belief" means all committed beliefs, and tt is engine-stamped so
/// it can never be future-dated — resolving against the wall clock instead
/// would silently hide the newest beliefs after a backward clock step.
fn expr2tt_spec(expr: Expr) -> Result<ValidityTs> {
    let vld_span = expr.span();
    match expr.eval_to_const()? {
        DataValue::Num(n) => {
            let microseconds = n.get_int_strict().ok_or_else(|| {
                FloatValiditySpecification(vld_span, TemporalAxis::Transaction, n.get_float())
            })?;
            Ok(ValidityTs(Reverse(microseconds)))
        }
        DataValue::Str(s) => match &s as &str {
            "NOW" | "END" => Ok(MAX_VALIDITY_TS),
            s => Ok(str2vld(s).map_err(|_| BadValiditySpecification(vld_span))?),
        },
        // mnestic fork (0.14.0): the typed bridge — `:as_of dt_to_validity(...)`
        // or any other expression producing a Validity carries its own unit.
        DataValue::Validity(vld) => Ok(vld.timestamp),
        _ => {
            bail!(BadValiditySpecification(vld_span))
        }
    }
}

/// Parse the interior of a validity clause (mnestic fork, bitemporality §4):
/// either the shipped bare expr (valid time, unchanged) or the labeled
/// order-free axis form `(vt: E)` / `(tt: E)` / `(vt: E1, tt: E2)`.
/// Returns `(vt_spec, tt_spec)`.
fn parse_temporal_clause(
    vld_clause: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<(Option<ValidityTs>, Option<ValidityTs>)> {
    let inner = vld_clause.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::temporal_axes => {
            #[derive(Debug, Error, Diagnostic)]
            #[error("duplicate temporal axis label in @ clause")]
            #[diagnostic(code(parser::duplicate_temporal_axis))]
            struct DupAxis(#[label] SourceSpan);

            let mut vt = None;
            let mut tt = None;
            for pair_p in inner.into_inner() {
                let span = pair_p.extract_span();
                let mut ps = pair_p.into_inner();
                let label = ps.next().unwrap().as_str().to_string();
                let expr = build_expr(ps.next().unwrap(), param_pool)?;
                match label.as_str() {
                    "vt" => {
                        if vt.replace(expr2vld_spec(expr, cur_vld)?).is_some() {
                            bail!(DupAxis(span));
                        }
                    }
                    "tt" => {
                        if tt.replace(expr2tt_spec(expr)?).is_some() {
                            bail!(DupAxis(span));
                        }
                    }
                    _ => unreachable!(),
                }
            }
            Ok((vt, tt))
        }
        _ => {
            let vld_expr = build_expr(inner, param_pool)?;
            Ok((Some(expr2vld_spec(vld_expr, cur_vld)?), None))
        }
    }
}

fn expr2vld_spec(expr: Expr, cur_vld: ValidityTs) -> Result<ValidityTs> {
    let vld_span = expr.span();
    match expr.eval_to_const()? {
        DataValue::Num(n) => {
            let microseconds = n.get_int_strict().ok_or_else(|| {
                FloatValiditySpecification(vld_span, TemporalAxis::Valid, n.get_float())
            })?;
            Ok(ValidityTs(Reverse(microseconds)))
        }
        DataValue::Str(s) => match &s as &str {
            "NOW" => Ok(cur_vld),
            "END" => Ok(MAX_VALIDITY_TS),
            s => Ok(str2vld(s).map_err(|_| BadValiditySpecification(vld_span))?),
        },
        // mnestic fork (0.14.0): the typed bridge — `@ dt_to_validity(...)` or
        // any other expression producing a Validity carries its own unit.
        DataValue::Validity(vld) => Ok(vld.timestamp),
        _ => {
            bail!(BadValiditySpecification(vld_span))
        }
    }
}
