/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Persisted named CozoScript queries (`::query`). Definitions are ordinary
//! relation rows, parsed afresh and hygienically spliced before normalization.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use itertools::Itertools;
use miette::{bail, ensure, IntoDiagnostic, Result, WrapErr};
use serde_json::json;
use smartstring::SmartString;

use crate::data::aggr::CustomAggrRegistries;
use crate::data::program::{
    FixedRuleArg, InputAtom, InputInlineRulesOrFixed, InputProgram, QueryOutOptions, ReorderMode,
};
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::symb::{Symbol, PROG_ENTRY};
use crate::data::value::{DataValue, JsonData, UuidWrapper, Validity, ValidityTs, Vector};
use crate::fixed_rule::FixedRule;
use crate::parse::sys::StoredQueryParam;
use crate::parse::{collect_query_params, parse_script, SourceSpan};
use crate::runtime::diagnostics;
use crate::runtime::relation::{AccessLevel, InputRelationHandle, RelationHandle};
use crate::runtime::transact::SessionTx;
use crate::NamedRows;

pub(crate) const STORED_QUERY_CATALOG: &str = "mnestic_stored_queries";
const MAX_REFERENCE_DEPTH: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct StoredQueryDefinition {
    pub(crate) name: String,
    pub(crate) body: String,
    pub(crate) params: Vec<StoredQueryParam>,
    pub(crate) head: Vec<String>,
    pub(crate) deps: Vec<String>,
    pub(crate) description: Option<String>,
    pub(crate) created_at: f64,
}

#[derive(Debug, Clone)]
struct RuleUse {
    name: String,
    arity: usize,
    span: SourceSpan,
}

#[derive(Debug, Clone)]
struct RewriteTarget {
    name: String,
    arity: usize,
}

fn catalog_metadata() -> StoredRelationMetadata {
    let col = |name: &str, coltype: ColType, nullable: bool| ColumnDef {
        name: SmartString::from(name),
        typing: NullableColType { coltype, nullable },
        default_gen: None,
    };
    StoredRelationMetadata {
        keys: vec![col("name", ColType::String, false)],
        non_keys: vec![
            col("body", ColType::String, false),
            col("params", ColType::Json, false),
            col("head", ColType::Json, false),
            col("deps", ColType::Json, false),
            col("description", ColType::String, true),
            col("created_at", ColType::Float, false),
        ],
    }
}

fn validate_catalog(handle: &RelationHandle) -> Result<()> {
    ensure!(
        handle.metadata == catalog_metadata()
            && handle.has_no_index()
            && handle.put_triggers.is_empty()
            && handle.rm_triggers.is_empty()
            && handle.replace_triggers.is_empty(),
        "the relation name {STORED_QUERY_CATALOG} is reserved for stored queries; \
         the existing relation must keep the exact reserved schema and carry no indices or triggers"
    );
    Ok(())
}

fn catalog_for_read(tx: &SessionTx<'_>) -> Result<Option<RelationHandle>> {
    if !tx.relation_exists(STORED_QUERY_CATALOG)? {
        return Ok(None);
    }
    let handle = tx.get_relation_for_read(STORED_QUERY_CATALOG, "stored-query catalog read")?;
    validate_catalog(&handle)?;
    Ok(Some(handle))
}

fn catalog_for_write(tx: &mut SessionTx<'_>) -> Result<RelationHandle> {
    let handle = match catalog_for_read(tx)? {
        Some(handle) => handle,
        None => tx.create_relation(InputRelationHandle {
            name: Symbol::new(STORED_QUERY_CATALOG, SourceSpan::default()),
            metadata: catalog_metadata(),
            key_bindings: vec![],
            dep_bindings: vec![],
            span: SourceSpan::default(),
        })?,
    };
    ensure!(
        handle.access_level == AccessLevel::Normal,
        "stored-query catalog '{STORED_QUERY_CATALOG}' is {}; set its access level to normal before modifying it",
        handle.access_level
    );
    tx.mark_dirty(&handle);
    Ok(handle)
}

fn json_value<T: serde::Serialize>(value: &T) -> Result<DataValue> {
    Ok(DataValue::Json(JsonData(
        serde_json::to_value(value).into_diagnostic()?,
    )))
}

fn decode_json<T: serde::de::DeserializeOwned>(value: &DataValue, field: &str) -> Result<T> {
    let DataValue::Json(value) = value else {
        bail!("corrupt stored-query catalog: '{field}' is not Json");
    };
    serde_json::from_value(value.0.clone())
        .into_diagnostic()
        .wrap_err_with(|| format!("corrupt stored-query catalog field '{field}'"))
}

fn decode_definition(tuple: Vec<DataValue>) -> Result<StoredQueryDefinition> {
    ensure!(
        tuple.len() == 7,
        "corrupt stored-query catalog row: expected 7 fields, got {}",
        tuple.len()
    );
    let string = |idx: usize, field: &str| -> Result<String> {
        match &tuple[idx] {
            DataValue::Str(value) => Ok(value.to_string()),
            _ => bail!("corrupt stored-query catalog: '{field}' is not String"),
        }
    };
    let description = match &tuple[5] {
        DataValue::Null => None,
        DataValue::Str(value) => Some(value.to_string()),
        _ => bail!("corrupt stored-query catalog: 'description' is not String?"),
    };
    let created_at = tuple[6].get_float().ok_or_else(|| {
        miette::miette!("corrupt stored-query catalog: 'created_at' is not Float")
    })?;
    Ok(StoredQueryDefinition {
        name: string(0, "name")?,
        body: string(1, "body")?,
        params: decode_json(&tuple[2], "params")?,
        head: decode_json(&tuple[3], "head")?,
        deps: decode_json(&tuple[4], "deps")?,
        description,
        created_at,
    })
}

pub(crate) fn read_definition(
    tx: &SessionTx<'_>,
    name: &str,
) -> Result<Option<StoredQueryDefinition>> {
    let Some(catalog) = catalog_for_read(tx)? else {
        return Ok(None);
    };
    catalog
        .get(tx, &[DataValue::from(name)])?
        .map(decode_definition)
        .transpose()
}

fn zero_value(typing: Option<&NullableColType>) -> DataValue {
    let Some(typing) = typing else {
        return DataValue::Null;
    };
    match &typing.coltype {
        ColType::Any => DataValue::Null,
        ColType::Bool => DataValue::from(false),
        ColType::Int => DataValue::from(0_i64),
        ColType::Float => DataValue::from(0_f64),
        ColType::String => DataValue::from(""),
        ColType::Bytes => DataValue::Bytes(Vec::new()),
        ColType::Uuid => DataValue::Uuid(UuidWrapper(uuid::Uuid::nil())),
        ColType::List { .. } => DataValue::List(Vec::new()),
        ColType::Tuple(types) => DataValue::List(
            types
                .iter()
                .map(|typing| zero_value(Some(typing)))
                .collect(),
        ),
        ColType::Vec { eltype, len } => match eltype {
            crate::data::relation::VecElementType::F32 => {
                DataValue::Vec(Vector::F32(ndarray::Array1::zeros(*len)))
            }
            crate::data::relation::VecElementType::F64 => {
                DataValue::Vec(Vector::F64(ndarray::Array1::zeros(*len)))
            }
        },
        ColType::Json => DataValue::Json(JsonData(json!(null))),
        ColType::Validity | ColType::TxTime => DataValue::Validity(Validity::from((0, true))),
    }
}

fn adjusted_param_pool(
    definition: &StoredQueryDefinition,
    invocation: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<BTreeMap<String, DataValue>> {
    let mut adjusted = invocation.clone();
    for param in &definition.params {
        let value = match invocation.get(&param.name) {
            Some(value) => value.clone(),
            None => param.default.clone().ok_or_else(|| {
                miette::miette!(
                    "stored query '{}' requires parameter '${}'",
                    definition.name,
                    param.name
                )
            })?,
        };
        let value = match &param.typing {
            Some(typing) => typing.coerce(value, cur_vld).wrap_err_with(|| {
                format!(
                    "stored query '{}' parameter '${}' does not satisfy {}",
                    definition.name, param.name, typing
                )
            })?,
            None => value,
        };
        adjusted.insert(param.name.clone(), value);
    }
    Ok(adjusted)
}

pub(crate) fn parse_definition(
    definition: &StoredQueryDefinition,
    invocation: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<Box<dyn FixedRule>>>,
    custom_aggrs: CustomAggrRegistries<'_>,
    cur_vld: ValidityTs,
) -> Result<InputProgram> {
    let adjusted = adjusted_param_pool(definition, invocation, cur_vld)?;
    parse_script(
        &definition.body,
        &adjusted,
        fixed_rules,
        custom_aggrs,
        cur_vld,
    )
    .and_then(|script| script.get_single_program())
    .wrap_err_with(|| format!("failed to parse stored query '{}'", definition.name))
}

fn walk_rule_uses(atom: &InputAtom, out: &mut Vec<RuleUse>) {
    match atom {
        InputAtom::Rule { inner } => out.push(RuleUse {
            name: inner.name.to_string(),
            arity: inner.args.len(),
            span: inner.span,
        }),
        InputAtom::Negation { inner, .. } => walk_rule_uses(inner, out),
        InputAtom::Conjunction { inner, .. } | InputAtom::Disjunction { inner, .. } => {
            for atom in inner {
                walk_rule_uses(atom, out);
            }
        }
        InputAtom::NamedFieldRelation { .. }
        | InputAtom::Relation { .. }
        | InputAtom::Predicate { .. }
        | InputAtom::Unification { .. }
        | InputAtom::Search { .. } => {}
    }
}

fn unresolved_rule_uses(program: &InputProgram) -> Vec<RuleUse> {
    let locals: BTreeSet<String> = program.prog.keys().map(ToString::to_string).collect();
    let mut uses = Vec::new();
    for rules in program.prog.values() {
        match rules {
            InputInlineRulesOrFixed::Rules { rules } => {
                for rule in rules {
                    for atom in &rule.body {
                        walk_rule_uses(atom, &mut uses);
                    }
                }
            }
            InputInlineRulesOrFixed::Fixed { fixed } => {
                for arg in &fixed.rule_args {
                    if let FixedRuleArg::InMem {
                        name,
                        bindings,
                        span,
                    } = arg
                    {
                        uses.push(RuleUse {
                            name: name.to_string(),
                            arity: bindings.len(),
                            span: *span,
                        });
                    }
                }
            }
        }
    }
    uses.into_iter()
        .filter(|usage| !locals.contains(&usage.name))
        .collect()
}

fn out_option_name(program: &InputProgram) -> Option<&'static str> {
    let opts: &QueryOutOptions = &program.out_opts;
    if opts.limit.is_some() {
        Some(":limit")
    } else if opts.offset.is_some() {
        Some(":offset")
    } else if opts.timeout.is_some() {
        Some(":timeout")
    } else if opts.mem_limit.is_some() {
        Some(":mem_limit")
    } else if opts.reorder != ReorderMode::Greedy {
        Some(":reorder")
    } else if opts.sleep.is_some() {
        Some(":sleep")
    } else if opts.as_of.is_some() {
        Some(":as_of")
    } else if !opts.sorters.is_empty() {
        Some(":order")
    } else if opts.store_relation.is_some() {
        Some("relation mutation")
    } else if opts.assertion.is_some() {
        Some(":assert")
    } else if program.disable_magic_rewrite {
        Some(":disable_magic_rewrite")
    } else {
        None
    }
}

fn rewrite_atom(
    atom: &mut InputAtom,
    local: &BTreeMap<String, RewriteTarget>,
    external: &BTreeMap<String, RewriteTarget>,
) -> Result<()> {
    match atom {
        InputAtom::Rule { inner } => {
            if let Some(target) = local
                .get(inner.name.name.as_str())
                .or_else(|| external.get(inner.name.name.as_str()))
            {
                ensure!(
                    inner.args.len() == target.arity,
                    "stored-query arity mismatch for '{}': definition has {} column(s), atom supplies {} at {}",
                    inner.name,
                    target.arity,
                    inner.args.len(),
                    inner.span
                );
                inner.name = Symbol::new(target.name.clone(), inner.name.span);
            }
        }
        InputAtom::Negation { inner, .. } => rewrite_atom(inner, local, external)?,
        InputAtom::Conjunction { inner, .. } | InputAtom::Disjunction { inner, .. } => {
            for atom in inner {
                rewrite_atom(atom, local, external)?;
            }
        }
        InputAtom::NamedFieldRelation { .. }
        | InputAtom::Relation { .. }
        | InputAtom::Predicate { .. }
        | InputAtom::Unification { .. }
        | InputAtom::Search { .. } => {}
    }
    Ok(())
}

fn rewrite_program(
    program: &mut InputProgram,
    local: &BTreeMap<String, RewriteTarget>,
    external: &BTreeMap<String, RewriteTarget>,
) -> Result<()> {
    for rules in program.prog.values_mut() {
        match rules {
            InputInlineRulesOrFixed::Rules { rules } => {
                for rule in rules {
                    for atom in &mut rule.body {
                        rewrite_atom(atom, local, external)?;
                    }
                }
            }
            InputInlineRulesOrFixed::Fixed { fixed } => {
                for arg in &mut fixed.rule_args {
                    if let FixedRuleArg::InMem {
                        name,
                        bindings,
                        span,
                    } = arg
                    {
                        if let Some(target) = local
                            .get(name.name.as_str())
                            .or_else(|| external.get(name.name.as_str()))
                        {
                            ensure!(
                                bindings.len() == target.arity,
                                "stored-query arity mismatch for '{}': definition has {} column(s), fixed-rule input supplies {} at {}",
                                name,
                                target.arity,
                                bindings.len(),
                                span
                            );
                            *name = Symbol::new(target.name.clone(), name.span);
                        }
                    }
                }
            }
        }
    }

    let old = std::mem::take(&mut program.prog);
    for (name, rules) in old {
        let rewritten = local
            .get(name.name.as_str())
            .map(|target| Symbol::new(target.name.clone(), name.span))
            .unwrap_or(name);
        program.prog.insert(rewritten, rules);
    }
    Ok(())
}

fn emit_shadow_warnings(tx: &SessionTx<'_>, program: &InputProgram) -> Result<()> {
    for local in program.prog.keys() {
        if local.name.as_str() == PROG_ENTRY || local.name.as_str().contains("::") {
            continue;
        }
        if read_definition(tx, local.name.as_str())?.is_some() {
            diagnostics::emit(
                "stored_query.local_shadow",
                format!("local rule '{}' shadows stored query '{}'", local, local),
                "rename the local rule to invoke the stored query from this program",
            );
        }
    }
    Ok(())
}

fn load_spliced(
    tx: &SessionTx<'_>,
    name: &str,
    invocation: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<Box<dyn FixedRule>>>,
    custom_aggrs: CustomAggrRegistries<'_>,
    cur_vld: ValidityTs,
    depth: usize,
    inserted: &mut BTreeSet<String>,
    additions: &mut BTreeMap<Symbol, InputInlineRulesOrFixed>,
) -> Result<StoredQueryDefinition> {
    ensure!(
        depth < MAX_REFERENCE_DEPTH,
        "stored-query reference chain exceeds depth {MAX_REFERENCE_DEPTH}"
    );
    let definition = read_definition(tx, name)?
        .ok_or_else(|| miette::miette!("stored query '{name}' no longer exists"))?;
    if inserted.contains(name) {
        return Ok(definition);
    }

    let mut program =
        parse_definition(&definition, invocation, fixed_rules, custom_aggrs, cur_vld)?;
    ensure!(
        program.needs_write_lock().is_none(),
        "stored query '{}' is not read-only",
        definition.name
    );
    if let Some(option) = out_option_name(&program) {
        bail!(
            "stored query '{}' carries `{option}` — invocable only via `::query run`",
            definition.name
        );
    }
    emit_shadow_warnings(tx, &program)?;

    let uses = unresolved_rule_uses(&program);
    let mut external = BTreeMap::new();
    for dep_name in uses.iter().map(|usage| usage.name.as_str()).unique() {
        let dependency = load_spliced(
            tx,
            dep_name,
            invocation,
            fixed_rules,
            custom_aggrs,
            cur_vld,
            depth + 1,
            inserted,
            additions,
        )?;
        external.insert(
            dep_name.to_string(),
            RewriteTarget {
                name: format!("{}::?", dependency.name),
                arity: dependency.head.len(),
            },
        );
    }

    let local: BTreeMap<String, RewriteTarget> = program
        .prog
        .iter()
        .map(|(rule_name, rules)| {
            let arity = match rules {
                InputInlineRulesOrFixed::Rules { rules } => rules.last().unwrap().head.len(),
                InputInlineRulesOrFixed::Fixed { fixed } => fixed.arity,
            };
            (
                rule_name.to_string(),
                RewriteTarget {
                    name: format!("{}::{}", definition.name, rule_name),
                    arity,
                },
            )
        })
        .collect();
    rewrite_program(&mut program, &local, &external)?;
    for (rule_name, rules) in program.prog {
        additions.entry(rule_name).or_insert(rules);
    }
    inserted.insert(name.to_string());
    Ok(definition)
}

pub(crate) fn resolve_stored_queries(
    tx: &SessionTx<'_>,
    program: &mut InputProgram,
    fixed_rules: &BTreeMap<String, Arc<Box<dyn FixedRule>>>,
    custom_aggrs: CustomAggrRegistries<'_>,
    cur_vld: ValidityTs,
) -> Result<()> {
    emit_shadow_warnings(tx, program)?;
    let uses = unresolved_rule_uses(program);
    if uses.is_empty() {
        return Ok(());
    }

    let invocation = program.param_pool.as_ref().clone();
    let mut inserted = BTreeSet::new();
    let mut additions = BTreeMap::new();
    let mut external = BTreeMap::new();
    for candidate in uses.iter().map(|usage| usage.name.as_str()).unique() {
        let Some(definition) = read_definition(tx, candidate)? else {
            continue;
        };
        load_spliced(
            tx,
            candidate,
            &invocation,
            fixed_rules,
            custom_aggrs,
            cur_vld,
            0,
            &mut inserted,
            &mut additions,
        )?;
        external.insert(
            candidate.to_string(),
            RewriteTarget {
                name: format!("{}::?", definition.name),
                arity: definition.head.len(),
            },
        );
    }
    rewrite_program(program, &BTreeMap::new(), &external)?;
    for (name, rules) in additions {
        program.prog.entry(name).or_insert(rules);
    }
    Ok(())
}

pub(crate) fn create(
    tx: &mut SessionTx<'_>,
    name: &Symbol,
    params: &[StoredQueryParam],
    body: &str,
    fixed_rules: &BTreeMap<String, Arc<Box<dyn FixedRule>>>,
    custom_aggrs: CustomAggrRegistries<'_>,
    cur_vld: ValidityTs,
) -> Result<NamedRows> {
    ensure!(
        name.name.as_str() != STORED_QUERY_CATALOG,
        "stored query name '{name}' is reserved"
    );
    ensure!(
        read_definition(tx, name.name.as_str())?.is_none(),
        "stored query '{name}' already exists; remove it before creating a replacement"
    );

    let used = collect_query_params(body)?;
    let declared: BTreeSet<String> = params.iter().map(|param| param.name.clone()).collect();
    let undeclared: Vec<String> = used.difference(&declared).cloned().collect();
    let unused: Vec<String> = declared.difference(&used).cloned().collect();
    ensure!(
        undeclared.is_empty(),
        "stored query '{name}' body references undeclared parameter(s): {}",
        undeclared.iter().map(|p| format!("${p}")).join(", ")
    );
    ensure!(
        unused.is_empty(),
        "stored query '{name}' declares unused parameter(s): {}",
        unused.iter().map(|p| format!("${p}")).join(", ")
    );

    let synthetic: BTreeMap<String, DataValue> = params
        .iter()
        .map(|param| {
            (
                param.name.clone(),
                param
                    .default
                    .clone()
                    .unwrap_or_else(|| zero_value(param.typing.as_ref())),
            )
        })
        .collect();
    let temporary = StoredQueryDefinition {
        name: name.to_string(),
        body: body.to_string(),
        params: params.to_vec(),
        head: Vec::new(),
        deps: Vec::new(),
        description: None,
        created_at: 0.0,
    };
    let program = parse_definition(&temporary, &synthetic, fixed_rules, custom_aggrs, cur_vld)?;
    ensure!(
        program.needs_write_lock().is_none(),
        "stored query '{name}' must be read-only"
    );
    let head = program
        .get_entry_out_head_or_default()?
        .into_iter()
        .map(|symbol| symbol.to_string())
        .collect_vec();
    let uses = unresolved_rule_uses(&program);
    let mut deps = Vec::new();
    for dep in uses.iter().map(|usage| usage.name.as_str()).unique() {
        let dependency = read_definition(tx, dep)?.ok_or_else(|| {
            miette::miette!("stored query '{name}' references missing rule or stored query '{dep}'")
        })?;
        for usage in uses.iter().filter(|usage| usage.name == dep) {
            ensure!(
                usage.arity == dependency.head.len(),
                "stored-query arity mismatch for '{dep}': definition has {} column(s), atom supplies {} at {}",
                dependency.head.len(),
                usage.arity,
                usage.span
            );
        }
        deps.push(dep.to_string());
    }
    deps.sort();

    let handle = catalog_for_write(tx)?;
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .into_diagnostic()?
        .as_secs_f64();
    let row = vec![
        DataValue::from(name.name.as_str()),
        DataValue::from(body),
        json_value(&params)?,
        json_value(&head)?,
        json_value(&deps)?,
        DataValue::Null,
        DataValue::from(created_at),
    ];
    let key = handle.encode_key_for_store(&row, name.span)?;
    let value = handle.encode_val_for_store(&row, name.span)?;
    tx.store_tx.put(&key, &value)?;
    Ok(NamedRows::new(
        vec!["status".to_string()],
        vec![vec![DataValue::from("OK")]],
    ))
}

pub(crate) fn remove(tx: &mut SessionTx<'_>, name: &Symbol) -> Result<NamedRows> {
    let definition = read_definition(tx, name.name.as_str())?
        .ok_or_else(|| miette::miette!("stored query '{name}' does not exist"))?;
    let handle = catalog_for_write(tx)?;
    for row in handle.scan_all(tx) {
        let dependent = decode_definition(row?)?;
        if dependent.name != definition.name
            && dependent.deps.iter().any(|dep| dep == &definition.name)
        {
            bail!(
                "cannot remove stored query '{}': stored query '{}' depends on it",
                definition.name,
                dependent.name
            );
        }
    }
    let key = handle.encode_key_for_store(&[DataValue::from(name.name.as_str())], name.span)?;
    tx.store_tx.del(&key)?;
    Ok(NamedRows::new(
        vec!["status".to_string()],
        vec![vec![DataValue::from("OK")]],
    ))
}

pub(crate) fn list(tx: &SessionTx<'_>) -> Result<NamedRows> {
    let Some(handle) = catalog_for_read(tx)? else {
        return Ok(NamedRows::new(
            vec![
                "name".to_string(),
                "params".to_string(),
                "head".to_string(),
                "deps".to_string(),
                "description".to_string(),
                "created_at".to_string(),
            ],
            Vec::new(),
        ));
    };
    let rows = handle
        .scan_all(tx)
        .map(|row| {
            let row = row?;
            Ok(vec![
                row[0].clone(),
                row[2].clone(),
                row[3].clone(),
                row[4].clone(),
                row[5].clone(),
                row[6].clone(),
            ])
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(NamedRows::new(
        vec![
            "name".to_string(),
            "params".to_string(),
            "head".to_string(),
            "deps".to_string(),
            "description".to_string(),
            "created_at".to_string(),
        ],
        rows,
    ))
}

pub(crate) fn show(tx: &SessionTx<'_>, name: &Symbol) -> Result<NamedRows> {
    let definition = read_definition(tx, name.name.as_str())?
        .ok_or_else(|| miette::miette!("stored query '{name}' does not exist"))?;
    Ok(NamedRows::new(
        vec![
            "name".to_string(),
            "body".to_string(),
            "params".to_string(),
            "head".to_string(),
            "deps".to_string(),
            "description".to_string(),
            "created_at".to_string(),
        ],
        vec![vec![
            DataValue::from(definition.name),
            DataValue::from(definition.body),
            json_value(&definition.params)?,
            json_value(&definition.head)?,
            json_value(&definition.deps)?,
            definition
                .description
                .map(DataValue::from)
                .unwrap_or(DataValue::Null),
            DataValue::from(definition.created_at),
        ]],
    ))
}
