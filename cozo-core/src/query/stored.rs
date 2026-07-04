/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use itertools::Itertools;
use miette::{bail, Diagnostic, IntoDiagnostic, Result, WrapErr};
use pest::Parser;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::{Bytecode, Expr};
use crate::data::program::{FixedRuleApply, InputInlineRulesOrFixed, InputProgram, RelationOp};
use crate::data::relation::{ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::symb::Symbol;
use crate::data::tuple::{Tuple, ENCODED_KEY_MIN_LEN};
use crate::data::value::{DataValue, ValidityTs};
use crate::fixed_rule::utilities::constant::Constant;
use crate::fixed_rule::FixedRuleHandle;
use crate::fts::tokenizer::TextAnalyzer;
use crate::parse::expr::build_expr;
use crate::parse::{parse_script, CozoScriptParser, Rule};
use crate::runtime::callback::{CallbackCollector, CallbackOp};
use crate::runtime::minhash_lsh::HashPermutations;
use crate::runtime::relation::{
    extend_tuple_from_v, AccessLevel, InputRelationHandle, InsufficientAccessLevel, RelationHandle,
};
use crate::runtime::transact::{PendingTtWrite, SessionTx};
use crate::storage::Storage;
use crate::{Db, NamedRows, SourceSpan, StoreTx};

#[derive(Debug, Error, Diagnostic)]
#[error("attempting to write into relation {0} of arity {1} with data of arity {2}")]
#[diagnostic(code(eval::relation_arity_mismatch))]
struct RelationArityMismatch(String, usize, usize);

impl<'a> SessionTx<'a> {
    pub(crate) fn execute_relation<'s, S: Storage<'s>>(
        &mut self,
        db: &Db<S>,
        res_iter: impl Iterator<Item = Tuple>,
        op: RelationOp,
        meta: &InputRelationHandle,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        propagate_triggers: bool,
        force_collect: &str,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut to_clear = vec![];
        let mut replaced_old_triggers = None;
        if op == RelationOp::Replace {
            if !propagate_triggers {
                #[derive(Debug, Error, Diagnostic)]
                #[error("replace op in trigger is not allowed: {0}")]
                #[diagnostic(code(eval::replace_in_trigger))]
                struct ReplaceInTrigger(String);
                bail!(ReplaceInTrigger(meta.name.to_string()))
            }
            if let Ok(old_handle) = self.get_relation(&meta.name, true) {
                if old_handle.has_txtime() {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("cannot replace TxTime relation {0}: replace would silently destroy its history")]
                    #[diagnostic(
                        code(eval::txtime_replace_forbidden),
                        help("append corrections with :put, delete keys with :rm (tt-only), or ::remove the relation explicitly")
                    )]
                    struct TxTimeReplaceForbidden(String);
                    bail!(TxTimeReplaceForbidden(meta.name.to_string()));
                }
                if !old_handle.indices.is_empty() {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("cannot replace relation {0} since it has indices")]
                    #[diagnostic(code(eval::replace_rel_with_indices))]
                    struct ReplaceRelationWithIndices(String);
                    bail!(ReplaceRelationWithIndices(old_handle.name.to_string()))
                }
                if old_handle.access_level < AccessLevel::Normal {
                    bail!(InsufficientAccessLevel(
                        old_handle.name.to_string(),
                        "relation replacement".to_string(),
                        old_handle.access_level
                    ));
                }
                if old_handle.has_triggers() {
                    replaced_old_triggers = Some((old_handle.put_triggers, old_handle.rm_triggers))
                }
                for trigger in &old_handle.replace_triggers {
                    let program = parse_script(
                        trigger,
                        &Default::default(),
                        &db.fixed_rules.read().unwrap(),
                        &Default::default(), // triggers: custom aggregates unsupported (R0)
                        cur_vld,
                    )?
                    .get_single_program()?;

                    let (_, cleanups) = db
                        .run_query(
                            self,
                            program,
                            cur_vld,
                            callback_targets,
                            callback_collector,
                            false,
                        )
                        .map_err(|err| {
                            if err.source_code().is_some() {
                                err
                            } else {
                                err.with_source_code(trigger.to_string())
                            }
                        })?;
                    to_clear.extend(cleanups);
                }
                let destroy_res = self.destroy_relation(&meta.name)?;
                if !meta.name.is_temp_store_name() {
                    to_clear.extend(destroy_res);
                }
            }
        }
        let mut relation_store = if op == RelationOp::Replace || op == RelationOp::Create {
            self.create_relation(meta.clone())?
        } else {
            self.get_relation(&meta.name, false)?
        };
        if let Some((old_put, old_retract)) = replaced_old_triggers {
            relation_store.put_triggers = old_put;
            relation_store.rm_triggers = old_retract;
        }
        let InputRelationHandle {
            metadata,
            key_bindings,
            dep_bindings,
            span,
            ..
        } = meta;

        match op {
            RelationOp::Rm | RelationOp::Delete => self.remove_from_relation(
                db,
                res_iter,
                headers,
                cur_vld,
                callback_targets,
                callback_collector,
                propagate_triggers,
                &mut to_clear,
                &relation_store,
                metadata,
                key_bindings,
                op == RelationOp::Delete,
                force_collect,
                *span,
            )?,
            RelationOp::Ensure => self.ensure_in_relation(
                res_iter,
                headers,
                cur_vld,
                &relation_store,
                metadata,
                key_bindings,
                *span,
            )?,
            RelationOp::EnsureNot => self.ensure_not_in_relation(
                res_iter,
                headers,
                cur_vld,
                &relation_store,
                metadata,
                key_bindings,
                *span,
            )?,
            RelationOp::Reconcile => self.reconcile_tt_relation(
                res_iter,
                headers,
                cur_vld,
                &relation_store,
                metadata,
                key_bindings,
                dep_bindings,
                callback_targets.contains(&relation_store.name)
                    || force_collect == relation_store.name,
                *span,
            )?,
            RelationOp::Update => self.update_in_relation(
                db,
                res_iter,
                headers,
                cur_vld,
                callback_targets,
                callback_collector,
                propagate_triggers,
                &mut to_clear,
                &relation_store,
                metadata,
                key_bindings,
                force_collect,
                *span,
            )?,
            RelationOp::Create | RelationOp::Replace | RelationOp::Put | RelationOp::Insert => self
                .put_into_relation(
                    db,
                    res_iter,
                    headers,
                    cur_vld,
                    callback_targets,
                    callback_collector,
                    propagate_triggers,
                    &mut to_clear,
                    &relation_store,
                    metadata,
                    key_bindings,
                    dep_bindings,
                    op == RelationOp::Insert,
                    matches!(op, RelationOp::Create | RelationOp::Replace),
                    force_collect,
                    *span,
                )?,
        };

        Ok(to_clear)
    }

    fn put_into_relation<'s, S: Storage<'s>>(
        &mut self,
        db: &Db<S>,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        propagate_triggers: bool,
        to_clear: &mut Vec<(Vec<u8>, Vec<u8>)>,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        dep_bindings: &[Symbol],
        is_insert: bool,
        is_create: bool,
        force_collect: &str,
        span: SourceSpan,
    ) -> Result<()> {
        let is_callback_target =
            callback_targets.contains(&relation_store.name) || force_collect == relation_store.name;

        if relation_store.access_level < AccessLevel::Protected {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row insertion".to_string(),
                relation_store.access_level
            ));
        }

        if relation_store.has_txtime() {
            return self.buffer_tt_puts(
                res_iter,
                headers,
                cur_vld,
                relation_store,
                metadata,
                key_bindings,
                dep_bindings,
                is_insert,
                is_create,
                is_callback_target,
                span,
            );
        }

        let mut key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let need_to_collect = !force_collect.is_empty()
            || (!relation_store.is_temp
                && (is_callback_target
                    || (propagate_triggers && !relation_store.put_triggers.is_empty())));
        let has_indices = !relation_store.indices.is_empty();
        let has_hnsw_indices = !relation_store.hnsw_indices.is_empty();
        let has_fts_indices = !relation_store.fts_indices.is_empty();
        let has_lsh_indices = !relation_store.lsh_indices.is_empty();
        let mut new_tuples: Vec<DataValue> = vec![];
        let mut old_tuples: Vec<DataValue> = vec![];

        let val_extractors = if metadata.non_keys.is_empty() {
            make_extractors(
                &relation_store.metadata.non_keys,
                &metadata.keys,
                key_bindings,
                headers,
            )?
        } else {
            make_extractors(
                &relation_store.metadata.non_keys,
                &metadata.non_keys,
                dep_bindings,
                headers,
            )?
        };
        key_extractors.extend(val_extractors);
        let mut stack = vec![];
        let hnsw_filters = Self::make_hnsw_filters(relation_store)?;
        let fts_lsh_processors = self.make_fts_lsh_processors(relation_store)?;
        let lsh_perms = self.make_lsh_hash_perms(relation_store);

        for tuple in res_iter {
            let extracted: Vec<DataValue> = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;

            let key = relation_store.encode_key_for_store(&extracted, span)?;

            if is_insert {
                let already_exists = if relation_store.is_temp {
                    self.temp_store_tx.exists(&key, true)?
                } else {
                    self.store_tx.exists(&key, true)?
                };

                if already_exists {
                    bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: extracted,
                        notice: "key exists in database".to_string()
                    });
                }
            }

            let val = relation_store.encode_val_for_store(&extracted, span)?;

            if need_to_collect
                || has_indices
                || has_hnsw_indices
                || has_fts_indices
                || has_lsh_indices
            {
                if let Some(existing) = self.store_tx.get(&key, false)? {
                    let mut tup = extracted[0..relation_store.metadata.keys.len()].to_vec();
                    extend_tuple_from_v(&mut tup, &existing);
                    if has_indices && extracted != tup {
                        self.update_in_index(relation_store, &extracted, &tup)?;
                        self.del_in_fts(relation_store, &mut stack, &fts_lsh_processors, &tup)?;
                        self.del_in_lsh(relation_store, &tup)?;
                    }

                    if need_to_collect {
                        old_tuples.push(DataValue::List(tup));
                    }
                } else if has_indices {
                    for (idx_rel, extractor) in relation_store.indices.values() {
                        let idx_tup_new = extractor
                            .iter()
                            .map(|i| extracted[*i].clone())
                            .collect_vec();
                        let encoded_new =
                            idx_rel.encode_key_for_store(&idx_tup_new, Default::default())?;
                        self.store_tx.put(&encoded_new, &[])?;
                    }
                }

                self.update_in_hnsw(relation_store, &mut stack, &hnsw_filters, &extracted)?;
                self.put_in_fts(relation_store, &mut stack, &fts_lsh_processors, &extracted)?;
                self.put_in_lsh(
                    relation_store,
                    &mut stack,
                    &fts_lsh_processors,
                    &extracted,
                    &lsh_perms,
                )?;

                if need_to_collect {
                    new_tuples.push(DataValue::List(extracted));
                }
            }

            if relation_store.is_temp {
                self.temp_store_tx.put(&key, &val)?;
            } else {
                self.store_tx.put(&key, &val)?;
            }
        }

        if need_to_collect && !new_tuples.is_empty() {
            self.collect_mutations(
                db,
                cur_vld,
                callback_targets,
                callback_collector,
                propagate_triggers,
                to_clear,
                relation_store,
                is_callback_target,
                new_tuples,
                old_tuples,
            )?;
        }
        Ok(())
    }

    fn put_in_fts(
        &mut self,
        rel_handle: &RelationHandle,
        stack: &mut Vec<DataValue>,
        processors: &BTreeMap<SmartString<LazyCompact>, (Arc<TextAnalyzer>, Vec<Bytecode>)>,
        new_kv: &[DataValue],
    ) -> Result<()> {
        for (k, (idx_handle, _)) in rel_handle.fts_indices.iter() {
            let (tokenizer, extractor) = processors.get(k).unwrap();
            self.put_fts_index_item(new_kv, extractor, stack, tokenizer, rel_handle, idx_handle)?;
        }
        Ok(())
    }

    fn del_in_fts(
        &mut self,
        rel_handle: &RelationHandle,
        stack: &mut Vec<DataValue>,
        processors: &BTreeMap<SmartString<LazyCompact>, (Arc<TextAnalyzer>, Vec<Bytecode>)>,
        old_kv: &[DataValue],
    ) -> Result<()> {
        for (k, (idx_handle, _)) in rel_handle.fts_indices.iter() {
            let (tokenizer, extractor) = processors.get(k).unwrap();
            self.del_fts_index_item(old_kv, extractor, stack, tokenizer, rel_handle, idx_handle)?;
        }
        Ok(())
    }

    fn put_in_lsh(
        &mut self,
        rel_handle: &RelationHandle,
        stack: &mut Vec<DataValue>,
        processors: &BTreeMap<SmartString<LazyCompact>, (Arc<TextAnalyzer>, Vec<Bytecode>)>,
        new_kv: &[DataValue],
        hash_perms_map: &BTreeMap<SmartString<LazyCompact>, HashPermutations>,
    ) -> Result<()> {
        for (k, (idx_handle, inv_idx_handle, manifest)) in rel_handle.lsh_indices.iter() {
            let (tokenizer, extractor) = processors.get(k).unwrap();
            self.put_lsh_index_item(
                new_kv,
                extractor,
                stack,
                tokenizer,
                rel_handle,
                idx_handle,
                inv_idx_handle,
                manifest,
                hash_perms_map.get(k).unwrap(),
            )?;
        }
        Ok(())
    }

    fn del_in_lsh(&mut self, rel_handle: &RelationHandle, old_kv: &[DataValue]) -> Result<()> {
        for (idx_handle, inv_idx_handle, _) in rel_handle.lsh_indices.values() {
            self.del_lsh_index_item(old_kv, None, idx_handle, inv_idx_handle)?;
        }
        Ok(())
    }

    fn update_in_hnsw(
        &mut self,
        relation_store: &RelationHandle,
        stack: &mut Vec<DataValue>,
        hnsw_filters: &BTreeMap<SmartString<LazyCompact>, Vec<Bytecode>>,
        new_kv: &[DataValue],
    ) -> Result<()> {
        for (name, (idx_handle, idx_manifest)) in relation_store.hnsw_indices.iter() {
            let filter = hnsw_filters.get(name);
            self.hnsw_put(
                idx_manifest,
                relation_store,
                idx_handle,
                filter,
                stack,
                new_kv,
            )?;
        }
        Ok(())
    }

    fn make_lsh_hash_perms(
        &self,
        relation_store: &RelationHandle,
    ) -> BTreeMap<SmartString<LazyCompact>, HashPermutations> {
        let mut perms = BTreeMap::new();
        for (name, (_, _, manifest)) in relation_store.lsh_indices.iter() {
            perms.insert(name.clone(), manifest.get_hash_perms());
        }
        perms
    }

    fn make_fts_lsh_processors(
        &self,
        relation_store: &RelationHandle,
    ) -> Result<BTreeMap<SmartString<LazyCompact>, (Arc<TextAnalyzer>, Vec<Bytecode>)>> {
        let mut processors = BTreeMap::new();
        for (name, (_, manifest)) in relation_store.fts_indices.iter() {
            let tokenizer = self
                .tokenizers
                .get(name, &manifest.tokenizer, &manifest.filters)?;

            let parsed = CozoScriptParser::parse(Rule::expr, &manifest.extractor)
                .into_diagnostic()?
                .next()
                .unwrap();
            let mut code_expr = build_expr(parsed, &Default::default())?;
            let binding_map = relation_store.raw_binding_map();
            code_expr.fill_binding_indices(&binding_map)?;
            let extractor = code_expr.compile()?;
            processors.insert(name.clone(), (tokenizer, extractor));
        }
        for (name, (_, _, manifest)) in relation_store.lsh_indices.iter() {
            let tokenizer = self
                .tokenizers
                .get(name, &manifest.tokenizer, &manifest.filters)?;

            let parsed = CozoScriptParser::parse(Rule::expr, &manifest.extractor)
                .into_diagnostic()?
                .next()
                .unwrap();
            let mut code_expr = build_expr(parsed, &Default::default())?;
            let binding_map = relation_store.raw_binding_map();
            code_expr.fill_binding_indices(&binding_map)?;
            let extractor = code_expr.compile()?;
            processors.insert(name.clone(), (tokenizer, extractor));
        }
        Ok(processors)
    }

    fn make_hnsw_filters(
        relation_store: &RelationHandle,
    ) -> Result<BTreeMap<SmartString<LazyCompact>, Vec<Bytecode>>> {
        let mut hnsw_filters = BTreeMap::new();
        for (name, (_, manifest)) in relation_store.hnsw_indices.iter() {
            if let Some(f_code) = &manifest.index_filter {
                let parsed = CozoScriptParser::parse(Rule::expr, f_code)
                    .into_diagnostic()?
                    .next()
                    .unwrap();
                let mut code_expr = build_expr(parsed, &Default::default())?;
                let binding_map = relation_store.raw_binding_map();
                code_expr.fill_binding_indices(&binding_map)?;
                hnsw_filters.insert(name.clone(), code_expr.compile()?);
            }
        }
        Ok(hnsw_filters)
    }

    fn update_in_relation<'s, S: Storage<'s>>(
        &mut self,
        db: &Db<S>,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        propagate_triggers: bool,
        to_clear: &mut Vec<(Vec<u8>, Vec<u8>)>,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        force_collect: &str,
        span: SourceSpan,
    ) -> Result<()> {
        let is_callback_target =
            callback_targets.contains(&relation_store.name) || force_collect == relation_store.name;

        if relation_store.access_level < AccessLevel::Protected {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row update".to_string(),
                relation_store.access_level
            ));
        }

        if relation_store.has_txtime() {
            return self.buffer_tt_update(
                res_iter,
                headers,
                cur_vld,
                relation_store,
                metadata,
                key_bindings,
                is_callback_target,
                span,
            );
        }

        let key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let need_to_collect = !force_collect.is_empty()
            || (!relation_store.is_temp
                && (is_callback_target
                    || (propagate_triggers && !relation_store.put_triggers.is_empty())));
        let has_indices = !relation_store.indices.is_empty();
        let has_hnsw_indices = !relation_store.hnsw_indices.is_empty();
        let has_fts_indices = !relation_store.fts_indices.is_empty();
        let has_lsh_indices = !relation_store.lsh_indices.is_empty();
        let mut new_tuples: Vec<DataValue> = vec![];
        let mut old_tuples: Vec<DataValue> = vec![];

        let val_extractors = make_update_extractors(
            &relation_store.metadata.non_keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let mut stack = vec![];
        let hnsw_filters = Self::make_hnsw_filters(relation_store)?;
        let fts_lsh_processors = self.make_fts_lsh_processors(relation_store)?;
        let lsh_perms = self.make_lsh_hash_perms(relation_store);

        for tuple in res_iter {
            let mut new_kv: Vec<DataValue> = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;

            let key = relation_store.encode_key_for_store(&new_kv, span)?;
            let original_val_bytes = if relation_store.is_temp {
                self.temp_store_tx.get(&key, true)?
            } else {
                self.store_tx.get(&key, true)?
            };
            let original_val: Tuple = match original_val_bytes {
                None => {
                    bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: new_kv,
                        notice: "key to update does not exist".to_string()
                    })
                }
                Some(v) => rmp_serde::from_slice(&v[ENCODED_KEY_MIN_LEN..]).unwrap(),
            };
            let mut old_kv = Vec::with_capacity(relation_store.arity());
            old_kv.extend_from_slice(&new_kv);
            old_kv.extend_from_slice(&original_val);
            new_kv.reserve_exact(relation_store.arity());
            for (i, extractor) in val_extractors.iter().enumerate() {
                match extractor {
                    None => {
                        new_kv.push(original_val[i].clone());
                    }
                    Some(ex) => {
                        let val = ex.extract_data(&tuple, cur_vld)?;
                        new_kv.push(val);
                    }
                }
            }
            let new_val = relation_store.encode_val_for_store(&new_kv, span)?;

            if need_to_collect
                || has_indices
                || has_hnsw_indices
                || has_fts_indices
                || has_lsh_indices
            {
                self.del_in_fts(relation_store, &mut stack, &fts_lsh_processors, &old_kv)?;
                self.del_in_lsh(relation_store, &old_kv)?;
                self.update_in_index(relation_store, &new_kv, &old_kv)?;

                if need_to_collect {
                    old_tuples.push(DataValue::List(old_kv));
                }

                self.update_in_hnsw(relation_store, &mut stack, &hnsw_filters, &new_kv)?;
                self.put_in_fts(relation_store, &mut stack, &fts_lsh_processors, &new_kv)?;
                self.put_in_lsh(
                    relation_store,
                    &mut stack,
                    &fts_lsh_processors,
                    &new_kv,
                    &lsh_perms,
                )?;

                if need_to_collect {
                    new_tuples.push(DataValue::List(new_kv));
                }
            }

            if relation_store.is_temp {
                self.temp_store_tx.put(&key, &new_val)?;
            } else {
                self.store_tx.put(&key, &new_val)?;
            }
        }

        if need_to_collect && !new_tuples.is_empty() {
            self.collect_mutations(
                db,
                cur_vld,
                callback_targets,
                callback_collector,
                propagate_triggers,
                to_clear,
                relation_store,
                is_callback_target,
                new_tuples,
                old_tuples,
            )?;
        }
        Ok(())
    }

    fn collect_mutations<'s, S: Storage<'s>>(
        &mut self,
        db: &Db<S>,
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        propagate_triggers: bool,
        to_clear: &mut Vec<(Vec<u8>, Vec<u8>)>,
        relation_store: &RelationHandle,
        is_callback_target: bool,
        new_tuples: Vec<DataValue>,
        old_tuples: Vec<DataValue>,
    ) -> Result<()> {
        let mut bindings = relation_store
            .metadata
            .keys
            .iter()
            .map(|k| Symbol::new(k.name.clone(), Default::default()))
            .collect_vec();
        let v_bindings = relation_store
            .metadata
            .non_keys
            .iter()
            .map(|k| Symbol::new(k.name.clone(), Default::default()));
        bindings.extend(v_bindings);

        let kv_bindings = bindings;
        if propagate_triggers {
            for trigger in &relation_store.put_triggers {
                let mut program = parse_script(
                    trigger,
                    &Default::default(),
                    &db.fixed_rules.read().unwrap(),
                    &Default::default(), // triggers: custom aggregates unsupported (R0)
                    cur_vld,
                )?
                .get_single_program()?;

                make_const_rule(
                    &mut program,
                    "_new",
                    kv_bindings.clone(),
                    new_tuples.to_vec(),
                );
                make_const_rule(
                    &mut program,
                    "_old",
                    kv_bindings.clone(),
                    old_tuples.to_vec(),
                );

                let (_, cleanups) = db
                    .run_query(
                        self,
                        program,
                        cur_vld,
                        callback_targets,
                        callback_collector,
                        false,
                    )
                    .map_err(|err| {
                        if err.source_code().is_some() {
                            err
                        } else {
                            err.with_source_code(format!("{trigger} "))
                        }
                    })?;
                to_clear.extend(cleanups);
            }
        }

        if is_callback_target {
            let target_collector = callback_collector
                .entry(relation_store.name.clone())
                .or_default();
            let headers = kv_bindings
                .into_iter()
                .map(|k| k.name.to_string())
                .collect_vec();
            target_collector.push((
                CallbackOp::Put,
                NamedRows::new(
                    headers.clone(),
                    new_tuples
                        .into_iter()
                        .map(|v| match v {
                            DataValue::List(l) => l,
                            _ => unreachable!(),
                        })
                        .collect_vec(),
                ),
                NamedRows::new(
                    headers,
                    old_tuples
                        .into_iter()
                        .map(|v| match v {
                            DataValue::List(l) => l,
                            _ => unreachable!(),
                        })
                        .collect_vec(),
                ),
            ))
        }
        Ok(())
    }

    fn update_in_index(
        &mut self,
        relation_store: &RelationHandle,
        new_kv: &[DataValue],
        old_kv: &[DataValue],
    ) -> Result<()> {
        for (idx_rel, idx_extractor) in relation_store.indices.values() {
            let idx_tup_old = idx_extractor
                .iter()
                .map(|i| old_kv[*i].clone())
                .collect_vec();
            let encoded_old = idx_rel.encode_key_for_store(&idx_tup_old, Default::default())?;
            self.store_tx.del(&encoded_old)?;

            let idx_tup_new = idx_extractor
                .iter()
                .map(|i| new_kv[*i].clone())
                .collect_vec();
            let encoded_new = idx_rel.encode_key_for_store(&idx_tup_new, Default::default())?;
            self.store_tx.put(&encoded_new, &[])?;
        }
        Ok(())
    }

    fn ensure_not_in_relation(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        span: SourceSpan,
    ) -> Result<()> {
        if relation_store.access_level < AccessLevel::ReadOnly {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row check".to_string(),
                relation_store.access_level
            ));
        }

        if relation_store.has_txtime() {
            return self.tt_ensure(
                res_iter,
                headers,
                cur_vld,
                relation_store,
                metadata,
                key_bindings,
                false,
                span,
            );
        }

        let key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        for tuple in res_iter {
            let extracted: Vec<DataValue> = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            let key = relation_store.encode_key_for_store(&extracted, span)?;
            let already_exists = if relation_store.is_temp {
                self.temp_store_tx.exists(&key, true)?
            } else {
                self.store_tx.exists(&key, true)?
            };
            if already_exists {
                bail!(TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: extracted,
                    notice: "key exists in database".to_string()
                })
            }
        }
        Ok(())
    }

    fn ensure_in_relation(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        span: SourceSpan,
    ) -> Result<()> {
        if relation_store.access_level < AccessLevel::ReadOnly {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row check".to_string(),
                relation_store.access_level
            ));
        }

        if relation_store.has_txtime() {
            return self.tt_ensure(
                res_iter,
                headers,
                cur_vld,
                relation_store,
                metadata,
                key_bindings,
                true,
                span,
            );
        }

        let mut key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let val_extractors = make_extractors(
            &relation_store.metadata.non_keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;
        key_extractors.extend(val_extractors);

        for tuple in res_iter {
            let extracted: Vec<DataValue> = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;

            let key = relation_store.encode_key_for_store(&extracted, span)?;
            let val = relation_store.encode_val_for_store(&extracted, span)?;

            let existing = if relation_store.is_temp {
                self.temp_store_tx.get(&key, true)?
            } else {
                self.store_tx.get(&key, true)?
            };
            match existing {
                None => {
                    bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: extracted,
                        notice: "key does not exist in database".to_string()
                    })
                }
                Some(v) => {
                    if &v as &[u8] != &val as &[u8] {
                        bail!(TransactAssertionFailure {
                            relation: relation_store.name.to_string(),
                            key: extracted,
                            notice: "key exists in database, but value does not match".to_string()
                        })
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolved current-belief row of a tt-stamped relation for one fully
    /// bound plain-key prefix (mnestic fork, bitemporality 4c): (vt = NOW,
    /// tt = current belief) on bitemporal relations; (tt = current belief)
    /// on tt-only ones.
    fn tt_current_row(
        &self,
        handle: &RelationHandle,
        plain: &Tuple,
        cur_vld: ValidityTs,
    ) -> Result<Option<Tuple>> {
        use crate::data::functions::MAX_VALIDITY_TS;
        if handle.is_tt_only() {
            let mut it = handle.skip_scan_prefix(self, plain, MAX_VALIDITY_TS);
            it.next().transpose()
        } else {
            let mut it = handle.bitemporal_scan_prefix(self, plain, Some(cur_vld), MAX_VALIDITY_TS);
            it.next().transpose()
        }
    }

    /// Does this transaction already hold a buffered write touching the key?
    fn tt_pending_conflict(&self, handle: &RelationHandle, plain: &[DataValue]) -> bool {
        self.pending_tt_writes
            .iter()
            .any(|w| w.handle.id == handle.id && w.rows.iter().any(|r| &r[..plain.len()] == plain))
    }

    /// Reject writes that name the engine-assigned TxTime column, and writes
    /// that need machinery a tt-stamped relation doesn't support yet
    /// (mnestic fork, bitemporality step 3).
    fn check_tt_write_shape(
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        is_callback_target: bool,
        what: &str,
    ) -> Result<()> {
        let tt_name = &relation_store
            .metadata
            .keys
            .last()
            .expect("tt relation has keys")
            .name;
        if metadata
            .keys
            .iter()
            .chain(metadata.non_keys.iter())
            .any(|c| &c.name == tt_name)
        {
            #[derive(Debug, Error, Diagnostic)]
            #[error("column {0} is engine-assigned at commit and cannot be supplied")]
            #[diagnostic(
                code(eval::txtime_user_supplied_col),
                help("omit the TxTime column from the {1} spec; the engine stamps it with the transaction's commit time")
            )]
            struct TxTimeColSupplied(String, String);
            bail!(TxTimeColSupplied(tt_name.to_string(), what.to_string()));
        }
        if is_callback_target {
            #[derive(Debug, Error, Diagnostic)]
            #[error(":returning and event callbacks are not yet supported on TxTime relation {0}")]
            #[diagnostic(code(eval::txtime_callbacks_unsupported))]
            struct TxTimeCallbacks(String);
            bail!(TxTimeCallbacks(relation_store.name.to_string()));
        }
        Ok(())
    }

    /// Bail if `relation_store` was already `:reconcile`d in this
    /// transaction (provenance semirings R3): the reconcile declared the
    /// relation's COMPLETE belief, and a later write in the same belief
    /// event would silently amend or contradict it — an idempotent
    /// reconcile buffers no rows, so the pending-write checks alone cannot
    /// witness the declaration.
    fn check_not_reconciled(&self, relation_store: &RelationHandle, what: &str) -> Result<()> {
        if self.reconciled_tt_relations.contains(&relation_store.id.0) {
            bail!(TransactAssertionFailure {
                relation: relation_store.name.to_string(),
                key: vec![],
                notice: format!(
                    "{what} after :reconcile in one transaction: the reconcile declared \
                     the relation's complete belief — no further writes"
                )
            });
        }
        Ok(())
    }

    /// `:reconcile` on a TxTime relation (mnestic fork, provenance semirings
    /// R3): declare the query output to BE the relation's new current
    /// belief. The engine diffs the output against the resolved current
    /// belief and buffers, as ONE belief event at commit-tt:
    /// - assertions for keys whose belief is new or changed,
    /// - retractions (tt-only) / vt-cessations with values copied
    ///   (bitemporal) for currently-believed keys absent from the output.
    ///
    /// Unchanged keys buffer nothing, so re-reconciling an identical output
    /// is a true no-op (no tt burned, no history bloat). Whole-relation
    /// semantics: the output must be the COMPLETE intended belief set —
    /// every believed key missing from it is retracted. This is the
    /// recompute-based truth-maintenance step: retract or append base
    /// facts, re-derive, `:reconcile` the derived (annotated) relation;
    /// `::history` and as-of reads then answer "what did we believe, and
    /// why, as of T" across the revision. Truth maintenance is USER-driven —
    /// there is no automatic base→derived propagation (incremental
    /// DRed-style maintenance is recorded future work).
    ///
    /// Caveats (documented contracts):
    /// - the relation admits no other write in the same transaction, before
    ///   OR after the reconcile (the declaration must stay complete);
    /// - like every tt write, the revision is invisible to later reads in
    ///   the same script (§5: one belief event per transaction);
    /// - value columns with non-constant defaults (`rand_*`, `now()`)
    ///   defeat idempotence if omitted from the spec — every run re-asserts
    ///   every key; supply such columns explicitly;
    /// - bitemporal inputs should carry explicit vt timestamps: `'NOW'`
    ///   mints a fresh vt-group every run (each reconcile then asserts the
    ///   new group and ceases the previous one);
    /// - cost is O(relation): the current belief is fully resolved and both
    ///   belief sets are held in memory for the diff.
    #[allow(clippy::too_many_arguments)]
    fn reconcile_tt_relation(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        dep_bindings: &[Symbol],
        is_callback_target: bool,
        span: SourceSpan,
    ) -> Result<()> {
        use crate::data::functions::MAX_VALIDITY_TS;
        if !relation_store.has_txtime() {
            #[derive(Debug, Error, Diagnostic)]
            #[error(":reconcile requires a TxTime relation, {0} is not")]
            #[diagnostic(
                code(eval::reconcile_needs_txtime),
                help("reconciliation diffs against the relation's current belief and records the revision in transaction time; for a plain relation use :replace")
            )]
            struct ReconcileNeedsTxTime(String);
            bail!(ReconcileNeedsTxTime(relation_store.name.to_string()));
        }
        if relation_store.access_level < AccessLevel::Protected {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "belief reconciliation".to_string(),
                relation_store.access_level
            ));
        }
        Self::check_tt_write_shape(relation_store, metadata, is_callback_target, ":reconcile")?;
        self.check_not_reconciled(relation_store, ":reconcile")?;
        // A reconcile declares the COMPLETE belief; an earlier pending write
        // in the same transaction would be invisible to the diff (it reads
        // the store) yet stamped alongside it, silently contradicting the
        // declaration.
        if self
            .pending_tt_writes
            .iter()
            .any(|w| w.handle.id == relation_store.id)
        {
            bail!(TransactAssertionFailure {
                relation: relation_store.name.to_string(),
                key: vec![],
                notice: ":reconcile must be its relation's only write in the transaction"
                    .to_string()
            });
        }

        let kpos = relation_store.metadata.keys.len() - 1;
        let is_bitemporal = kpos >= 1
            && matches!(
                relation_store.metadata.keys[kpos - 1].typing.coltype,
                crate::data::relation::ColType::Validity
            );

        let mut extractors = make_extractors(
            &relation_store.metadata.keys[..kpos],
            &metadata.keys,
            key_bindings,
            headers,
        )?;
        let val_extractors = if metadata.non_keys.is_empty() {
            make_extractors(
                &relation_store.metadata.non_keys,
                &metadata.keys,
                key_bindings,
                headers,
            )?
        } else {
            make_extractors(
                &relation_store.metadata.non_keys,
                &metadata.non_keys,
                dep_bindings,
                headers,
            )?
        };
        extractors.extend(val_extractors);

        // the DESIRED belief set
        let mut desired: BTreeMap<Tuple, Tuple> = BTreeMap::new();
        for tuple in res_iter {
            let extracted: Vec<DataValue> = extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            let (prefix, vals) = extracted.split_at(kpos);
            if is_bitemporal {
                match &prefix[kpos - 1] {
                    DataValue::Validity(v) if v.is_assert.0 => {}
                    _ => bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: prefix.to_vec(),
                        notice: ":reconcile rows declare beliefs — the valid-time flag must \
                                 be ASSERT; cessations are computed from the diff"
                            .to_string()
                    }),
                }
            }
            match desired.entry(prefix.to_vec()) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(vals.to_vec());
                }
                std::collections::btree_map::Entry::Occupied(e) => {
                    if e.get().as_slice() != vals {
                        bail!(TransactAssertionFailure {
                            relation: relation_store.name.to_string(),
                            key: prefix.to_vec(),
                            notice: "conflicting rows for one key in a single :reconcile"
                                .to_string()
                        });
                    }
                }
            }
        }

        // the CURRENT belief set
        let mut current: BTreeMap<Tuple, Tuple> = BTreeMap::new();
        if is_bitemporal {
            // resolve-groups at current tt; a group whose belief is a
            // cessation surfaces retract-flagged and is NOT a belief
            for row in relation_store.bitemporal_scan_all(self, None, MAX_VALIDITY_TS) {
                let row = row?;
                match &row[kpos - 1] {
                    DataValue::Validity(v) if !v.is_assert.0 => continue,
                    _ => {}
                }
                current.insert(row[..kpos].to_vec(), row[kpos + 1..].to_vec());
            }
        } else {
            // per-key latest record, believed-deleted skipped
            for row in relation_store.skip_scan_all(self, MAX_VALIDITY_TS) {
                let row = row?;
                current.insert(row[..kpos].to_vec(), row[kpos + 1..].to_vec());
            }
        }

        // the diff — one belief event
        let mut asserts: Vec<Tuple> = Vec::new();
        let mut retracts: Vec<Tuple> = Vec::new();
        for (prefix, vals) in &desired {
            if current.get(prefix) == Some(vals) {
                continue;
            }
            let mut row = prefix.clone();
            row.extend(vals.iter().cloned());
            asserts.push(row);
        }
        for (prefix, vals) in &current {
            if desired.contains_key(prefix) {
                continue;
            }
            let mut row = prefix.clone();
            if is_bitemporal {
                // cessation in the belief's own vt-group, values copied
                let vt_ts = match &row[kpos - 1] {
                    DataValue::Validity(v) => v.timestamp,
                    _ => unreachable!("vt column decodes to Validity"),
                };
                row[kpos - 1] = DataValue::Validity(crate::data::value::Validity {
                    timestamp: vt_ts,
                    is_assert: std::cmp::Reverse(false),
                });
                row.extend(vals.iter().cloned());
                // the retract flag rides the vt axis
                asserts.push(row);
            } else {
                row.extend(vals.iter().cloned());
                retracts.push(row);
            }
        }
        if !asserts.is_empty() {
            self.pending_tt_writes.push(PendingTtWrite {
                handle: relation_store.clone(),
                rows: asserts,
                is_retract: false,
                span,
            });
        }
        if !retracts.is_empty() {
            self.pending_tt_writes.push(PendingTtWrite {
                handle: relation_store.clone(),
                rows: retracts,
                is_retract: true,
                span,
            });
        }
        // registered even when nothing was buffered: the declaration itself
        // must be witnessable by later statements
        self.reconciled_tt_relations.insert(relation_store.id.0);
        Ok(())
    }

    /// Buffer `:put`s into a tt-stamped relation (mnestic fork, bitemporality
    /// step 3): rows are extracted now but stamped and written at commit
    /// (`SessionTx::stamp_pending_tt_writes`), so the whole transaction
    /// shares one engine-assigned tt. Not visible to later reads in the same
    /// script (spec §5 — one belief event per transaction).
    #[allow(clippy::too_many_arguments)]
    fn buffer_tt_puts(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        dep_bindings: &[Symbol],
        is_insert: bool,
        is_create: bool,
        is_callback_target: bool,
        span: SourceSpan,
    ) -> Result<()> {
        self.check_not_reconciled(relation_store, ":put")?;
        // On :create the input metadata IS the declared schema (tt included,
        // legitimately); the supplied-column check applies to put specs only —
        // but the query HEADERS must still not smuggle a tt value in.
        if !is_create {
            Self::check_tt_write_shape(relation_store, metadata, is_callback_target, ":put")?;
        } else {
            if is_callback_target {
                Self::check_tt_write_shape(
                    relation_store,
                    &StoredRelationMetadata {
                        keys: vec![],
                        non_keys: vec![],
                    },
                    is_callback_target,
                    ":create",
                )?;
            }
        }

        let kpos = relation_store.metadata.keys.len() - 1;
        let mut extractors = make_extractors(
            &relation_store.metadata.keys[..kpos],
            &metadata.keys,
            key_bindings,
            headers,
        )?;
        let val_extractors = if metadata.non_keys.is_empty() {
            make_extractors(
                &relation_store.metadata.non_keys,
                &metadata.keys,
                key_bindings,
                headers,
            )?
        } else {
            make_extractors(
                &relation_store.metadata.non_keys,
                &metadata.non_keys,
                dep_bindings,
                headers,
            )?
        };
        extractors.extend(val_extractors);

        let tt_name = &relation_store
            .metadata
            .keys
            .last()
            .expect("tt relation has keys")
            .name;
        // On a bare :create the headers legitimately include the declared tt
        // column but there are no rows; :create-with-rows supplying tt data
        // must NOT silently drop it.
        let headers_have_tt = is_create && headers.iter().any(|h| &h.name == tt_name);

        let is_bitemporal = kpos >= 1
            && matches!(
                relation_store.metadata.keys[kpos - 1].typing.coltype,
                crate::data::relation::ColType::Validity
            );
        let plain_len = if is_bitemporal { kpos - 1 } else { kpos };

        let mut rows = Vec::new();
        let mut seen_in_stmt: std::collections::BTreeSet<Vec<u8>> = Default::default();
        for tuple in res_iter {
            if headers_have_tt {
                #[derive(Debug, Error, Diagnostic)]
                #[error("column {0} is engine-assigned at commit and cannot be supplied")]
                #[diagnostic(
                    code(eval::txtime_user_supplied_col),
                    help("remove the TxTime column from the :create input rows; the engine stamps it")
                )]
                struct TxTimeHeaderSupplied(String);
                bail!(TxTimeHeaderSupplied(tt_name.to_string()));
            }
            let extracted: Vec<DataValue> = extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            if is_insert {
                // :insert (mnestic fork, 4c): duplicates within one
                // statement would silently last-write-win — reject them.
                let plain: Tuple = extracted[..plain_len].to_vec();
                if !seen_in_stmt.insert(relation_store.encode_partial_key_for_store(&plain)) {
                    bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: plain,
                        notice: "duplicate key in one :insert statement".to_string()
                    });
                }
                if self.tt_pending_conflict(relation_store, &plain) {
                    bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: plain,
                        notice: "key was already written (or removed) in this transaction"
                            .to_string()
                    });
                }
                // tt-only: no CURRENT belief may exist (re-inserting a
                // believed-deleted key stays legal). Bitemporal: no records
                // at ANY valid time — a (vt=NOW)-only gate would let :insert
                // silently rewrite past/future vt-groups (review-probed).
                let exists = if is_bitemporal {
                    let mut it = relation_store.scan_prefix(self, &plain);
                    it.next().transpose()?.is_some()
                } else {
                    self.tt_current_row(relation_store, &plain, cur_vld)?
                        .is_some()
                };
                if exists {
                    bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: plain,
                        notice: if is_bitemporal {
                            "key has recorded beliefs (use :put for versions/corrections)"
                                .to_string()
                        } else {
                            "key exists in database (current belief)".to_string()
                        }
                    });
                }
            }
            rows.push(extracted);
        }
        if !rows.is_empty() {
            self.pending_tt_writes.push(PendingTtWrite {
                handle: relation_store.clone(),
                rows,
                is_retract: false,
                span,
            });
        }
        Ok(())
    }

    /// Buffer `:rm`/`:delete` on a tt-stamped relation (mnestic fork,
    /// bitemporality step 3). On tt-only relations this appends a retraction
    /// at commit-tt — never a physical delete; values are taken from the
    /// key's current (latest-tt) row. On bitemporal relations removal is
    /// expressed on the vt axis instead (`:put` with `"RETRACT"`), so `:rm`
    /// directs there until the read path lands.
    #[allow(clippy::too_many_arguments)]
    fn buffer_tt_removals(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        check_exists: bool,
        is_callback_target: bool,
        span: SourceSpan,
    ) -> Result<()> {
        self.check_not_reconciled(relation_store, ":rm")?;
        let kpos = relation_store.metadata.keys.len() - 1;
        let is_bitemporal = kpos >= 1
            && matches!(
                relation_store.metadata.keys[kpos - 1].typing.coltype,
                crate::data::relation::ColType::Validity
            );
        Self::check_tt_write_shape(relation_store, metadata, is_callback_target, ":rm")?;
        if is_bitemporal {
            // :rm {k, vt} on a bitemporal relation (mnestic fork, 4c; spec
            // §6): a cessation — equivalent to `:put` with vt RETRACT at the
            // supplied valid time, with values taken from the key's belief at
            // that vt. No belief there → :rm is a no-op (:delete asserts).
            return self.buffer_bitemporal_rm(
                res_iter,
                headers,
                cur_vld,
                relation_store,
                metadata,
                key_bindings,
                check_exists,
                span,
                kpos,
            );
        }

        let extractors = make_extractors(
            &relation_store.metadata.keys[..kpos],
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let mut prefixes = Vec::new();
        for tuple in res_iter {
            let extracted: Vec<DataValue> = extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            prefixes.push(extracted);
        }

        // A key written earlier in this same transaction cannot also be
        // removed by it — one belief event per transaction (the stamp-time
        // cross-check covers the reverse order; this catches rm-after-put
        // with a clearer message).
        for prefix in &prefixes {
            let clashes = self.pending_tt_writes.iter().any(|w| {
                !w.is_retract
                    && w.handle.id == relation_store.id
                    && w.rows
                        .iter()
                        .any(|r| &r[0..prefix.len()] == prefix.as_slice())
            });
            if clashes {
                #[derive(Debug, Error, Diagnostic)]
                #[error(
                    "cannot remove a key written in the same transaction from TxTime relation {0}"
                )]
                #[diagnostic(
                    code(eval::txtime_rm_pending_put),
                    help("one belief event per transaction: both rows would carry the same transaction time; split into two transactions")
                )]
                struct TtRmPendingPut(String);
                bail!(TtRmPendingPut(relation_store.name.to_string()));
            }
        }

        // Values for the retraction rows come from each key's current
        // (latest-tt) row — collected before mutating self to satisfy the
        // borrow on the scan iterator.
        let mut rows = Vec::new();
        for prefix in prefixes {
            let existing: Option<Tuple> = {
                let mut it = relation_store.scan_prefix(self, &prefix.to_vec());
                match it.next() {
                    None => None,
                    Some(t) => Some(t?),
                }
            };
            match existing {
                None => {
                    if check_exists {
                        bail!(TransactAssertionFailure {
                            relation: relation_store.name.to_string(),
                            key: prefix,
                            notice: "key does not exist in database".to_string()
                        });
                    }
                    // :rm of a missing key is a no-op, as on plain relations.
                }
                Some(full) => {
                    // Already believed-deleted? (latest row is a retraction)
                    if let Some(DataValue::Validity(v)) = full.get(kpos) {
                        if !v.is_assert.0 {
                            if check_exists {
                                bail!(TransactAssertionFailure {
                                    relation: relation_store.name.to_string(),
                                    key: prefix,
                                    notice: "key is believed-deleted".to_string()
                                });
                            }
                            continue;
                        }
                    }
                    let mut row = prefix;
                    row.extend_from_slice(&full[kpos + 1..]);
                    rows.push(row);
                }
            }
        }
        if !rows.is_empty() {
            self.pending_tt_writes.push(PendingTtWrite {
                handle: relation_store.clone(),
                rows,
                is_retract: true,
                span,
            });
        }
        Ok(())
    }

    /// Bitemporal `:rm {k, vt}` (mnestic fork, 4c): buffer a vt-retraction
    /// row at the supplied valid time, values from the key's belief there.
    #[allow(clippy::too_many_arguments)]
    fn buffer_bitemporal_rm(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        check_exists: bool,
        span: SourceSpan,
        kpos: usize,
    ) -> Result<()> {
        self.check_not_reconciled(relation_store, ":rm")?;
        use crate::data::functions::MAX_VALIDITY_TS;
        // keys minus tt: plain keys + the vt column (user-supplied)
        let extractors = make_extractors(
            &relation_store.metadata.keys[..kpos],
            &metadata.keys,
            key_bindings,
            headers,
        )?;
        let mut prefixes = Vec::new();
        for tuple in res_iter {
            let extracted: Vec<DataValue> = extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            prefixes.push(extracted);
        }

        for prefix in &prefixes {
            let plain = &prefix[..kpos - 1];
            if self.tt_pending_conflict(relation_store, plain) {
                bail!(TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: prefix.clone(),
                    notice: "key was already written (or removed) in this transaction".to_string()
                });
            }
        }

        let mut rows = Vec::new();
        for mut prefix in prefixes {
            let vt_ts = match &prefix[kpos - 1] {
                DataValue::Validity(v) => v.timestamp,
                _ => unreachable!("vt column coerces to Validity"),
            };
            let existing: Option<Tuple> = {
                let plain: Tuple = prefix[..kpos - 1].to_vec();
                let mut it = relation_store.bitemporal_scan_prefix(
                    self,
                    &plain,
                    Some(vt_ts),
                    MAX_VALIDITY_TS,
                );
                it.next().transpose()?
            };
            match existing {
                None => {
                    if check_exists {
                        bail!(TransactAssertionFailure {
                            relation: relation_store.name.to_string(),
                            key: prefix,
                            notice: "no belief exists at that valid time".to_string()
                        });
                    }
                }
                Some(full) => {
                    // retraction row: user vt with the retract flag, values
                    // copied from the ceasing belief
                    prefix[kpos - 1] = DataValue::Validity(crate::data::value::Validity {
                        timestamp: vt_ts,
                        is_assert: std::cmp::Reverse(false),
                    });
                    let mut row = prefix;
                    row.extend_from_slice(&full[kpos + 1..]);
                    rows.push(row);
                }
            }
        }
        if !rows.is_empty() {
            self.pending_tt_writes.push(PendingTtWrite {
                handle: relation_store.clone(),
                rows,
                is_retract: false, // bitemporal: the retract flag rides vt
                span,
            });
        }
        Ok(())
    }

    /// `:update` on a tt-stamped relation (mnestic fork, 4c; spec §6):
    /// read the resolved current belief, merge the provided value columns,
    /// buffer the merged row as a correction at commit-tt (bitemporal: in
    /// the current belief's own vt-group).
    #[allow(clippy::too_many_arguments)]
    fn buffer_tt_update(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        is_callback_target: bool,
        span: SourceSpan,
    ) -> Result<()> {
        self.check_not_reconciled(relation_store, ":update")?;
        Self::check_tt_write_shape(relation_store, metadata, is_callback_target, ":update")?;
        let kpos = relation_store.metadata.keys.len() - 1;
        let is_bitemporal = kpos >= 1
            && matches!(
                relation_store.metadata.keys[kpos - 1].typing.coltype,
                crate::data::relation::ColType::Validity
            );
        let plain_len = if is_bitemporal { kpos - 1 } else { kpos };
        if is_bitemporal
            && metadata
                .keys
                .iter()
                .any(|c| c.name == relation_store.metadata.keys[kpos - 1].name)
        {
            bail!(
                ":update on bitemporal relation {} targets the current belief; \
                 to correct a specific valid-time version use :put",
                relation_store.name
            );
        }
        let key_extractors = make_extractors(
            &relation_store.metadata.keys[..plain_len],
            &metadata.keys,
            key_bindings,
            headers,
        )?;
        let val_extractors = make_update_extractors(
            &relation_store.metadata.non_keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let mut inputs = Vec::new();
        for tuple in res_iter {
            let plain: Vec<DataValue> = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            let provided: Vec<Option<DataValue>> = val_extractors
                .iter()
                .map(|ex| -> Result<Option<DataValue>> {
                    Ok(match ex {
                        Some(e) => Some(e.extract_data(&tuple, cur_vld)?),
                        None => None,
                    })
                })
                .try_collect()?;
            inputs.push((plain, provided));
        }

        let mut rows = Vec::new();
        for (plain, provided) in inputs {
            if self.tt_pending_conflict(relation_store, &plain) {
                bail!(TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: plain,
                    notice: "key was already written (or removed) in this transaction".to_string()
                });
            }
            let current = self
                .tt_current_row(relation_store, &plain.to_vec(), cur_vld)?
                .ok_or_else(|| TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: plain.clone(),
                    notice: "key does not exist in database (current belief)".to_string(),
                })?;
            // resolved row layout: [plain.., (vt,) tt, vals..]
            let mut row = plain;
            if is_bitemporal {
                // correction lands in the current belief's own vt-group
                row.push(current[kpos - 1].clone());
            }
            for (i, p) in provided.into_iter().enumerate() {
                row.push(match p {
                    Some(v) => v,
                    None => current[kpos + 1 + i].clone(),
                });
            }
            rows.push(row);
        }
        if !rows.is_empty() {
            self.pending_tt_writes.push(PendingTtWrite {
                handle: relation_store.clone(),
                rows,
                is_retract: false,
                span,
            });
        }
        Ok(())
    }

    /// `:ensure` / `:ensure_not` on a tt-stamped relation (mnestic fork, 4c):
    /// assertions against the resolved current belief; this transaction's
    /// own buffered rows count as existing.
    #[allow(clippy::too_many_arguments)]
    fn tt_ensure(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        must_exist: bool,
        span: SourceSpan,
    ) -> Result<()> {
        let _ = span;
        Self::check_tt_write_shape(relation_store, metadata, false, ":ensure")?;
        let kpos = relation_store.metadata.keys.len() - 1;
        let is_bitemporal = kpos >= 1
            && matches!(
                relation_store.metadata.keys[kpos - 1].typing.coltype,
                crate::data::relation::ColType::Validity
            );
        let plain_len = if is_bitemporal { kpos - 1 } else { kpos };
        if is_bitemporal
            && metadata
                .keys
                .iter()
                .chain(metadata.non_keys.iter())
                .any(|c| c.name == relation_store.metadata.keys[kpos - 1].name)
        {
            bail!(
                ":ensure/:ensure_not on bitemporal relation {} assert about the CURRENT belief; the valid-time column cannot be bound",
                relation_store.name
            );
        }
        let key_extractors = make_extractors(
            &relation_store.metadata.keys[..plain_len],
            &metadata.keys,
            key_bindings,
            headers,
        )?;
        let val_extractors = if must_exist {
            make_update_extractors(
                &relation_store.metadata.non_keys,
                &metadata.keys,
                key_bindings,
                headers,
            )?
        } else {
            vec![]
        };
        let mut checks = Vec::new();
        for tuple in res_iter {
            let plain: Vec<DataValue> = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            let provided: Vec<Option<DataValue>> = val_extractors
                .iter()
                .map(|ex| -> Result<Option<DataValue>> {
                    Ok(match ex {
                        Some(e) => Some(e.extract_data(&tuple, cur_vld)?),
                        None => None,
                    })
                })
                .try_collect()?;
            checks.push((plain, provided));
        }
        for (plain, provided) in checks {
            let pending = self.tt_pending_conflict(relation_store, &plain);
            let current = self.tt_current_row(relation_store, &plain.to_vec(), cur_vld)?;
            let exists = pending || current.is_some();
            if must_exist {
                if pending && current.is_some() {
                    bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: plain,
                        notice: "key is being rewritten in this transaction — ambiguous assertion target; assert in a separate transaction".to_string()
                    });
                }
                let current = current.ok_or_else(|| TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: plain.clone(),
                    notice: if pending {
                        "key is only written in this transaction (not yet committed)".to_string()
                    } else {
                        "key does not exist in database (current belief)".to_string()
                    },
                })?;
                for (i, p) in provided.into_iter().enumerate() {
                    if let Some(v) = p {
                        let cur_v = &current[kpos + 1 + i];
                        if &v != cur_v {
                            bail!(TransactAssertionFailure {
                                relation: relation_store.name.to_string(),
                                key: plain,
                                notice: format!(
                                    "value mismatch: expected {v:?}, current belief {cur_v:?}"
                                )
                            });
                        }
                    }
                }
            } else if exists {
                bail!(TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: plain,
                    notice: "key exists (current belief, or written/removed by this transaction)"
                        .to_string()
                });
            }
        }
        Ok(())
    }

    fn remove_from_relation<'s, S: Storage<'s>>(
        &mut self,
        db: &Db<S>,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        propagate_triggers: bool,
        to_clear: &mut Vec<(Vec<u8>, Vec<u8>)>,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        check_exists: bool,
        force_collect: &str,
        span: SourceSpan,
    ) -> Result<()> {
        let is_callback_target =
            callback_targets.contains(&relation_store.name) || force_collect == relation_store.name;

        if relation_store.access_level < AccessLevel::Protected {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row removal".to_string(),
                relation_store.access_level
            ));
        }

        if relation_store.has_txtime() {
            return self.buffer_tt_removals(
                res_iter,
                headers,
                cur_vld,
                relation_store,
                metadata,
                key_bindings,
                check_exists,
                is_callback_target,
                span,
            );
        }

        let key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let need_to_collect = !force_collect.is_empty()
            || (!relation_store.is_temp
                && (is_callback_target
                    || (propagate_triggers && !relation_store.rm_triggers.is_empty())));
        let has_indices = !relation_store.indices.is_empty();
        let has_hnsw_indices = !relation_store.hnsw_indices.is_empty();
        let has_fts_indices = !relation_store.fts_indices.is_empty();
        let has_lsh_indices = !relation_store.lsh_indices.is_empty();
        let fts_processors = self.make_fts_lsh_processors(relation_store)?;
        let mut new_tuples: Vec<DataValue> = vec![];
        let mut old_tuples: Vec<DataValue> = vec![];
        let mut stack = vec![];

        for tuple in res_iter {
            let extracted: Vec<DataValue> = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            let key = relation_store.encode_key_for_store(&extracted, span)?;
            if check_exists {
                let exists = if relation_store.is_temp {
                    self.temp_store_tx.exists(&key, false)?
                } else {
                    self.store_tx.exists(&key, false)?
                };
                if !exists {
                    bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: extracted,
                        notice: "key does not exists in database".to_string()
                    });
                }
            }
            if need_to_collect
                || has_indices
                || has_hnsw_indices
                || has_fts_indices
                || has_lsh_indices
            {
                if let Some(existing) = self.store_tx.get(&key, false)? {
                    let mut tup = extracted.clone();
                    extend_tuple_from_v(&mut tup, &existing);
                    self.del_in_fts(relation_store, &mut stack, &fts_processors, &tup)?;
                    self.del_in_lsh(relation_store, &tup)?;
                    if has_indices {
                        for (idx_rel, extractor) in relation_store.indices.values() {
                            let idx_tup = extractor.iter().map(|i| tup[*i].clone()).collect_vec();
                            let encoded =
                                idx_rel.encode_key_for_store(&idx_tup, Default::default())?;
                            self.store_tx.del(&encoded)?;
                        }
                    }
                    if has_hnsw_indices {
                        for (idx_handle, _) in relation_store.hnsw_indices.values() {
                            self.hnsw_remove(relation_store, idx_handle, &extracted)?;
                        }
                    }
                    if need_to_collect {
                        old_tuples.push(DataValue::List(tup));
                    }
                }
                if need_to_collect {
                    new_tuples.push(DataValue::List(extracted.clone()));
                }
            }
            if relation_store.is_temp {
                self.temp_store_tx.del(&key)?;
            } else {
                self.store_tx.del(&key)?;
            }
        }

        // triggers and callbacks
        if need_to_collect && !new_tuples.is_empty() {
            let k_bindings = relation_store
                .metadata
                .keys
                .iter()
                .map(|k| Symbol::new(k.name.clone(), Default::default()))
                .collect_vec();

            let v_bindings = relation_store
                .metadata
                .non_keys
                .iter()
                .map(|k| Symbol::new(k.name.clone(), Default::default()));
            let mut kv_bindings = k_bindings.clone();
            kv_bindings.extend(v_bindings);
            let kv_bindings = kv_bindings;

            if propagate_triggers {
                for trigger in &relation_store.rm_triggers {
                    let mut program = parse_script(
                        trigger,
                        &Default::default(),
                        &db.fixed_rules.read().unwrap(),
                        &Default::default(), // triggers: custom aggregates unsupported (R0)
                        cur_vld,
                    )?
                    .get_single_program()?;

                    make_const_rule(&mut program, "_new", k_bindings.clone(), new_tuples.clone());

                    make_const_rule(
                        &mut program,
                        "_old",
                        kv_bindings.clone(),
                        old_tuples.clone(),
                    );

                    let (_, cleanups) = db
                        .run_query(
                            self,
                            program,
                            cur_vld,
                            callback_targets,
                            callback_collector,
                            false,
                        )
                        .map_err(|err| {
                            if err.source_code().is_some() {
                                err
                            } else {
                                err.with_source_code(format!("{trigger} "))
                            }
                        })?;
                    to_clear.extend(cleanups);
                }
            }

            if is_callback_target {
                let target_collector = callback_collector
                    .entry(relation_store.name.clone())
                    .or_default();
                target_collector.push((
                    CallbackOp::Rm,
                    NamedRows::new(
                        k_bindings
                            .into_iter()
                            .map(|k| k.name.to_string())
                            .collect_vec(),
                        new_tuples
                            .into_iter()
                            .map(|v| match v {
                                DataValue::List(l) => l,
                                _ => unreachable!(),
                            })
                            .collect_vec(),
                    ),
                    NamedRows::new(
                        kv_bindings
                            .into_iter()
                            .map(|k| k.name.to_string())
                            .collect_vec(),
                        old_tuples
                            .into_iter()
                            .map(|v| match v {
                                DataValue::List(l) => l,
                                _ => unreachable!(),
                            })
                            .collect_vec(),
                    ),
                ))
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Assertion failure for {key:?} of {relation}: {notice}")]
#[diagnostic(code(transact::assertion_failure))]
struct TransactAssertionFailure {
    relation: String,
    key: Vec<DataValue>,
    notice: String,
}

enum DataExtractor {
    DefaultExtractor(Expr, NullableColType),
    IndexExtractor(usize, NullableColType),
}

impl DataExtractor {
    fn extract_data(&self, tuple: &Tuple, cur_vld: ValidityTs) -> Result<DataValue> {
        Ok(match self {
            DataExtractor::DefaultExtractor(expr, typ) => typ
                .coerce(expr.clone().eval_to_const()?, cur_vld)
                .wrap_err_with(|| format!("when processing tuple {tuple:?}"))?,
            DataExtractor::IndexExtractor(i, typ) => typ
                .coerce(tuple[*i].clone(), cur_vld)
                .wrap_err_with(|| format!("when processing tuple {tuple:?}"))?,
        })
    }
}

fn make_extractors(
    stored: &[ColumnDef],
    input: &[ColumnDef],
    bindings: &[Symbol],
    tuple_headers: &[Symbol],
) -> Result<Vec<DataExtractor>> {
    stored
        .iter()
        .map(|s| make_extractor(s, input, bindings, tuple_headers))
        .try_collect()
}

fn make_update_extractors(
    stored: &[ColumnDef],
    input: &[ColumnDef],
    bindings: &[Symbol],
    tuple_headers: &[Symbol],
) -> Result<Vec<Option<DataExtractor>>> {
    let input_keys: BTreeSet<_> = input.iter().map(|b| &b.name).collect();
    let mut extractors = Vec::with_capacity(stored.len());
    for col in stored.iter() {
        if input_keys.contains(&col.name) {
            extractors.push(Some(make_extractor(col, input, bindings, tuple_headers)?));
        } else {
            extractors.push(None);
        }
    }
    Ok(extractors)
}

fn make_extractor(
    stored: &ColumnDef,
    input: &[ColumnDef],
    bindings: &[Symbol],
    tuple_headers: &[Symbol],
) -> Result<DataExtractor> {
    for (inp_col, inp_binding) in input.iter().zip(bindings.iter()) {
        if inp_col.name == stored.name {
            for (idx, tuple_head) in tuple_headers.iter().enumerate() {
                if tuple_head == inp_binding {
                    return Ok(DataExtractor::IndexExtractor(idx, stored.typing.clone()));
                }
            }
        }
    }
    if let Some(expr) = &stored.default_gen {
        Ok(DataExtractor::DefaultExtractor(
            expr.clone(),
            stored.typing.clone(),
        ))
    } else {
        #[derive(Debug, Error, Diagnostic)]
        #[error("cannot make extractor for column {0}")]
        #[diagnostic(code(eval::unable_to_make_extractor))]
        struct UnableToMakeExtractor(String);
        Err(UnableToMakeExtractor(stored.name.to_string()).into())
    }
}

fn make_const_rule(
    program: &mut InputProgram,
    rule_name: &str,
    bindings: Vec<Symbol>,
    data: Vec<DataValue>,
) {
    let rule_symbol = Symbol::new(SmartString::from(rule_name), Default::default());
    let mut options = BTreeMap::new();
    options.insert(
        SmartString::from("data"),
        Expr::Const {
            val: DataValue::List(data),
            span: Default::default(),
        },
    );
    let bindings_arity = bindings.len();
    program.prog.insert(
        rule_symbol,
        InputInlineRulesOrFixed::Fixed {
            fixed: FixedRuleApply {
                fixed_handle: FixedRuleHandle {
                    name: Symbol::new("Constant", Default::default()),
                },
                rule_args: vec![],
                options: Arc::new(options),
                head: bindings,
                arity: bindings_arity,
                span: Default::default(),
                fixed_impl: Arc::new(Box::new(Constant)),
            },
        },
    );
}
