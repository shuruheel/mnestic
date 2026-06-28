/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Translator: Cypher AST + property-graph schema -> CozoScript string + params.
//!
//! The output runs through the normal read-only query path. Literals become
//! params (`$cphr_N`) so nothing user-supplied is interpolated; every identifier
//! from the schema and from user Cypher is validated. Bag semantics are preserved
//! via a hidden binding-key in the head (set semantics over distinct bindings =
//! the Cypher bag); `RETURN DISTINCT` and aggregation collapse as Cypher
//! specifies. WHERE is lowered through a null-aware (three-valued-logic) emitter
//! so a null operand excludes the row rather than aborting the query. See
//! `docs/specs/cypher-read.md` §2–§6 and the review fix plan in
//! `docs/specs/cypher-read-review-findings.json`.
//!
//! v1 scope (errors, not silent gaps, for the rest): directed relationships,
//! labels (or unlabeled over a single shared node relation), WHERE, RETURN
//! (DISTINCT / bag / aggregates), inline-property filters, edge-isomorphism,
//! ORDER BY / SKIP / LIMIT over projected columns. Deferred: undirected
//! relationships, the schema `filter` field, variable-length paths, OPTIONAL
//! MATCH, WITH. Known divergence: `sum` over an integer column returns a float
//! (the engine's accumulator is f64).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use miette::{bail, Result};

use super::ast::*;
use super::schema::{CypherGraphSchema, EdgeMap, NodeMap};
use crate::data::value::DataValue;

/// A translated Cypher query: the CozoScript source, the extracted literal
/// params, and the user-visible output column names (in order). In bag mode the
/// script head may carry extra trailing binding-key columns; the runner keeps
/// the first `out_columns.len()` columns.
pub(crate) struct CypherScript {
    pub script: String,
    pub params: BTreeMap<String, DataValue>,
    pub out_columns: Vec<String>,
}

/// Translate a parsed Cypher query against a property-graph schema.
pub(crate) fn cypher_to_script(
    query: &CypherQuery,
    schema: &CypherGraphSchema,
) -> Result<CypherScript> {
    Translator::new(schema).run(query)
}

struct NodeB {
    var: String,
    label: Option<String>,
    props: Vec<(String, CExpr)>,
}

struct RelB {
    clause: usize,
    rel_type: Option<String>,
    user_var: Option<String>,
    from_v: String,
    to_v: String,
    props: Vec<(String, CExpr)>,
}

/// Per-relationship identity info used for edge-isomorphism disequalities.
struct RelId {
    clause: usize,
    relation: String,
    type_disc: Option<String>,
    eid_var: Option<String>,
    from_v: String,
    to_v: String,
}

struct Translator<'s> {
    schema: &'s CypherGraphSchema,
    params: BTreeMap<String, DataValue>,
    pcount: usize,
    anon: usize,
    /// Interned, injective names for (variable, property) pairs.
    prop_names: BTreeMap<(String, String), String>,
}

impl<'s> Translator<'s> {
    fn new(schema: &'s CypherGraphSchema) -> Self {
        Translator {
            schema,
            params: BTreeMap::new(),
            pcount: 0,
            anon: 0,
            prop_names: BTreeMap::new(),
        }
    }

    fn param(&mut self, v: DataValue) -> String {
        let name = format!("cphr_{}", self.pcount);
        self.pcount += 1;
        self.params.insert(name.clone(), v);
        format!("${name}")
    }

    fn fresh(&mut self, prefix: &str) -> String {
        let v = format!("{prefix}{}", self.anon);
        self.anon += 1;
        v
    }

    /// Interned, injective binding variable for a (var, key) property access.
    /// Underscore-joining `var` and `key` would be ambiguous (cyp_a_b_c could be
    /// (a,"b_c") or (a_b,"c")), so each distinct pair gets an opaque `cyp_N`.
    fn pvar(&mut self, var: &str, key: &str) -> String {
        let k = (var.to_string(), key.to_string());
        if let Some(n) = self.prop_names.get(&k) {
            return n.clone();
        }
        let name = format!("cyp_{}", self.prop_names.len());
        self.prop_names.insert(k, name.clone());
        name
    }

    fn run(&mut self, query: &CypherQuery) -> Result<CypherScript> {
        let (nodes, rels) = self.collect(query)?;
        self.validate_bindings(query, &nodes, &rels)?;

        // Properties referenced anywhere must be bound in the node/rel access;
        // those used in RETURN/ORDER must also be exposed in the cy_match head.
        let mut all_props = Vec::new();
        let mut head_props = Vec::new();
        for rc in &query.reading {
            if let Some(p) = &rc.where_pred {
                collect_props(p, &mut all_props);
            }
        }
        for item in &query.ret.items {
            collect_props(&item.expr, &mut all_props);
            collect_props(&item.expr, &mut head_props);
        }
        for s in &query.ret.order_by {
            collect_props(&s.expr, &mut all_props);
            collect_props(&s.expr, &mut head_props);
        }

        // --- cy_match body ---
        let mut body: Vec<String> = Vec::new();
        for n in &nodes {
            self.node_atom(n, &all_props, &mut body)?;
        }
        let mut rel_ids: Vec<RelId> = Vec::new();
        for r in &rels {
            let id = self.rel_atom(r, &all_props, &mut body)?;
            rel_ids.push(id);
        }
        // WHERE filters, lowered null-aware (keep only exactly-TRUE rows).
        for rc in &query.reading {
            if let Some(p) = &rc.where_pred {
                body.push(self.truthy(p)?);
            }
        }
        // Edge-isomorphism: within one MATCH, two relationships on the same
        // stored relation AND the same type must be distinct edges.
        for i in 0..rel_ids.len() {
            for j in (i + 1)..rel_ids.len() {
                let a = &rel_ids[i];
                let b = &rel_ids[j];
                if a.clause != b.clause || a.relation != b.relation || a.type_disc != b.type_disc {
                    continue;
                }
                match (&a.eid_var, &b.eid_var) {
                    (Some(x), Some(y)) => body.push(format!("{x} != {y}")),
                    _ => body.push(format!(
                        "({} != {} or {} != {})",
                        a.from_v, b.from_v, a.to_v, b.to_v
                    )),
                }
            }
        }

        // Structural binding key: node vars + rel eid vars (never property vars).
        let mut bind_key: Vec<String> = nodes.iter().map(|n| n.var.clone()).collect();
        for id in &rel_ids {
            if let Some(e) = &id.eid_var {
                push_unique(&mut bind_key, e.clone());
            }
        }
        // cy_match head = binding key + property vars needed downstream.
        let mut match_head = bind_key.clone();
        for (var, key) in &head_props {
            let pv = self.pvar(var, key);
            push_unique(&mut match_head, pv);
        }

        let mut script = String::new();
        writeln!(
            script,
            "cy_match[{}] := {}",
            match_head.join(", "),
            body.join(", ")
        )
        .unwrap();

        let has_aggr = query
            .ret
            .items
            .iter()
            .any(|it| matches!(&it.expr, CExpr::Func { name, .. } if is_aggregate(name)));

        let out_columns: Vec<String> = query
            .ret
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                it.alias
                    .clone()
                    .unwrap_or_else(|| default_col_name(&it.expr, i))
            })
            .collect();

        if has_aggr {
            self.emit_aggregate(&mut script, query, &match_head, &nodes)?;
        } else {
            self.emit_projection(&mut script, query, &match_head, &bind_key)?;
        }

        self.emit_epilogue(&mut script, &query.ret)?;

        Ok(CypherScript {
            script,
            params: std::mem::take(&mut self.params),
            out_columns,
        })
    }

    /// Push a node relation access atom (and any equality constraints) onto `body`.
    fn node_atom(
        &mut self,
        n: &NodeB,
        all_props: &[(String, String)],
        body: &mut Vec<String>,
    ) -> Result<()> {
        let nm = self.resolve_node(n)?;
        reject_filter(nm.filter.as_ref(), "NodeMap")?;
        ident_ok(&nm.relation)?;
        ident_ok(&nm.id_col)?;
        let mut args = vec![format!("{}: {}", nm.id_col, n.var)];
        let mut label_param: Option<String> = None;
        if let Some(lc) = &nm.label_col {
            ident_ok(lc)?;
            let v = nm
                .label_value
                .clone()
                .unwrap_or_else(|| DataValue::Str(n.label.clone().unwrap_or_default().into()));
            let p = self.param(v);
            args.push(format!("{}: {}", lc, p));
            label_param = Some(p);
        }

        let referenced: Vec<String> = all_props
            .iter()
            .filter(|(v, _)| *v == n.var)
            .map(|(_, k)| k.clone())
            .collect();
        let mut cols: Vec<String> = Vec::new();
        for k in &referenced {
            ident_ok(k)?;
            push_unique(&mut cols, k.clone());
        }
        for (k, _) in &n.props {
            ident_ok(k)?;
            push_unique(&mut cols, k.clone());
        }

        let mut eqs: Vec<String> = Vec::new();
        for col in &cols {
            let is_ref = referenced.contains(col);
            let inline: Vec<&CExpr> = n.props.iter().filter(|(k, _)| k == col).map(|(_, v)| v).collect();
            if *col == nm.id_col {
                // The id column is already bound to the node var; never re-bind it.
                if is_ref {
                    let pv = self.pvar(&n.var, col);
                    eqs.push(format!("{pv} = {}", n.var));
                }
                for v in &inline {
                    let e = self.expr(v)?;
                    eqs.push(format!("{} == {e}", n.var));
                }
            } else if nm.label_col.as_deref() == Some(col.as_str()) {
                // The discriminator already constrains this column.
                if is_ref {
                    if let Some(p) = &label_param {
                        let pv = self.pvar(&n.var, col);
                        eqs.push(format!("{pv} = {p}"));
                    }
                }
            } else if is_ref {
                let pv = self.pvar(&n.var, col);
                args.push(format!("{col}: {pv}"));
                for v in &inline {
                    let e = self.expr(v)?;
                    eqs.push(format!("{pv} == {e}"));
                }
            } else if inline.len() == 1 {
                let e = self.expr(inline[0])?;
                args.push(format!("{col}: {e}")); // direct filter -> keyed lookup (#1)
            } else {
                // Multiple inline constraints on one column: bind once, equate each.
                let pv = self.pvar(&n.var, col);
                args.push(format!("{col}: {pv}"));
                for v in &inline {
                    let e = self.expr(v)?;
                    eqs.push(format!("{pv} == {e}"));
                }
            }
        }

        body.push(format!("*{}{{{}}}", nm.relation, args.join(", ")));
        body.extend(eqs);
        Ok(())
    }

    /// Push an edge relation access atom (+ equalities) onto `body`; return its id info.
    fn rel_atom(
        &mut self,
        r: &RelB,
        all_props: &[(String, String)],
        body: &mut Vec<String>,
    ) -> Result<RelId> {
        let em = self.resolve_edge(r)?;
        reject_filter(em.filter.as_ref(), "EdgeMap")?;
        ident_ok(&em.relation)?;
        ident_ok(&em.from_col)?;
        ident_ok(&em.to_col)?;
        let mut args = vec![
            format!("{}: {}", em.from_col, r.from_v),
            format!("{}: {}", em.to_col, r.to_v),
        ];

        let eid_var = match &em.eid_col {
            Some(ec) => {
                ident_ok(ec)?;
                let v = r.user_var.clone().unwrap_or_else(|| self.fresh("cyr"));
                args.push(format!("{}: {}", ec, v));
                Some(v)
            }
            None => None,
        };

        let type_disc = if em.type_col.is_some() {
            Some(
                em.type_value
                    .as_ref()
                    .and_then(|v| v.get_str().map(|s| s.to_string()))
                    .or_else(|| r.rel_type.clone())
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        if let Some(tc) = &em.type_col {
            ident_ok(tc)?;
            let v = em
                .type_value
                .clone()
                .unwrap_or_else(|| DataValue::Str(r.rel_type.clone().unwrap_or_default().into()));
            args.push(format!("{}: {}", tc, self.param(v)));
        }

        // Relationship property columns: referenced (bind) and inline (filter).
        let rel_var = r.user_var.as_deref();
        let referenced: Vec<String> = match rel_var {
            Some(rv) => all_props
                .iter()
                .filter(|(v, _)| v == rv)
                .map(|(_, k)| k.clone())
                .collect(),
            None => Vec::new(),
        };
        let mut cols: Vec<String> = Vec::new();
        for k in &referenced {
            ident_ok(k)?;
            push_unique(&mut cols, k.clone());
        }
        for (k, _) in &r.props {
            ident_ok(k)?;
            push_unique(&mut cols, k.clone());
        }

        let mut eqs: Vec<String> = Vec::new();
        for col in &cols {
            let is_ref = rel_var.is_some() && referenced.contains(col);
            let inline: Vec<&CExpr> = r.props.iter().filter(|(k, _)| k == col).map(|(_, v)| v).collect();
            if is_ref {
                let pv = self.pvar(rel_var.unwrap(), col);
                args.push(format!("{col}: {pv}"));
                for v in &inline {
                    let e = self.expr(v)?;
                    eqs.push(format!("{pv} == {e}"));
                }
            } else if inline.len() == 1 {
                let e = self.expr(inline[0])?;
                args.push(format!("{col}: {e}"));
            } else {
                let placeholder = self.fresh("cyrp");
                args.push(format!("{col}: {placeholder}"));
                for v in &inline {
                    let e = self.expr(v)?;
                    eqs.push(format!("{placeholder} == {e}"));
                }
            }
        }

        body.push(format!("*{}{{{}}}", em.relation, args.join(", ")));
        body.extend(eqs);
        Ok(RelId {
            clause: r.clause,
            relation: em.relation.clone(),
            type_disc,
            eid_var,
            from_v: r.from_v.clone(),
            to_v: r.to_v.clone(),
        })
    }

    fn emit_projection(
        &mut self,
        script: &mut String,
        query: &CypherQuery,
        match_head: &[String],
        bind_key: &[String],
    ) -> Result<()> {
        let mut ret_vars = Vec::new();
        let mut unifies = Vec::new();
        for (i, item) in query.ret.items.iter().enumerate() {
            let var = format!("cy_ret_{i}");
            unifies.push(format!("{var} = {}", self.expr(&item.expr)?));
            ret_vars.push(var);
        }
        // Bag mode: append the binding key (hidden) so duplicate projected rows
        // survive. DISTINCT omits it (Datalog set dedup = Cypher DISTINCT).
        let mut head = ret_vars.clone();
        if !query.ret.distinct {
            for v in bind_key {
                push_unique(&mut head, v.clone());
            }
        }
        let mut rule_body = vec![format!("cy_match[{}]", match_head.join(", "))];
        rule_body.extend(unifies);
        writeln!(script, "?[{}] := {}", head.join(", "), rule_body.join(", ")).unwrap();
        Ok(())
    }

    fn emit_aggregate(
        &mut self,
        script: &mut String,
        query: &CypherQuery,
        match_head: &[String],
        nodes: &[NodeB],
    ) -> Result<()> {
        let anchor = nodes
            .first()
            .map(|n| n.var.clone())
            .ok_or_else(|| miette::miette!("aggregation requires at least one node"))?;

        let mut agg_head: Vec<String> = Vec::new();
        let mut group_unifies: Vec<String> = Vec::new();
        let mut guards: Vec<String> = Vec::new();
        for (i, item) in query.ret.items.iter().enumerate() {
            if let CExpr::Func { name, args } = &item.expr {
                if is_aggregate(name) {
                    let (arg, is_star) = self.aggregate_arg(args, &anchor)?;
                    // Skip nulls per openCypher (and stop sum/avg aborting on null).
                    if !is_star {
                        guards.push(format!("!is_null({arg})"));
                    }
                    agg_head.push(format!("{}({arg})", cozo_aggr(name)));
                    continue;
                }
            }
            let gv = format!("cy_g_{i}");
            group_unifies.push(format!("{gv} = {}", self.expr(&item.expr)?));
            agg_head.push(gv);
        }

        let mut agg_body = vec![format!("cy_match[{}]", match_head.join(", "))];
        agg_body.extend(group_unifies);
        agg_body.extend(guards);
        writeln!(
            script,
            "cy_agg[{}] := {}",
            agg_head.join(", "),
            agg_body.join(", ")
        )
        .unwrap();

        let cols: Vec<String> = (0..query.ret.items.len())
            .map(|i| item_head_var(&query.ret, i, true))
            .collect();
        writeln!(script, "?[{}] := cy_agg[{}]", cols.join(", "), cols.join(", ")).unwrap();
        Ok(())
    }

    fn aggregate_arg(&mut self, args: &FuncArgs, anchor: &str) -> Result<(String, bool)> {
        match args {
            FuncArgs::Star => Ok((anchor.to_string(), true)),
            FuncArgs::Exprs(es) => {
                if es.len() != 1 {
                    bail!("aggregate functions take exactly one argument in v1");
                }
                match &es[0] {
                    CExpr::Var(v) => {
                        ident_ok(v)?;
                        Ok((v.clone(), false))
                    }
                    CExpr::Prop { var, key } => {
                        ident_ok(key)?;
                        Ok((self.pvar(var, key), false))
                    }
                    _ => bail!("aggregate argument must be a variable or property in v1"),
                }
            }
        }
    }

    fn emit_epilogue(&mut self, script: &mut String, ret: &ReturnClause) -> Result<()> {
        if !ret.order_by.is_empty() {
            let mut parts = Vec::new();
            for s in &ret.order_by {
                let col = self.order_var(&s.expr, ret)?;
                parts.push(format!("{}{}", if s.descending { "-" } else { "+" }, col));
            }
            writeln!(script, ":order {}", parts.join(", ")).unwrap();
        }
        if let Some(sk) = &ret.skip {
            writeln!(script, ":offset {}", const_int(sk)?).unwrap();
        }
        if let Some(lm) = &ret.limit {
            writeln!(script, ":limit {}", const_int(lm)?).unwrap();
        }
        Ok(())
    }

    /// Resolve an ORDER BY term to a head variable: it must match a RETURN item
    /// (by alias or by identical expression).
    fn order_var(&self, e: &CExpr, ret: &ReturnClause) -> Result<String> {
        let has_aggr = ret
            .items
            .iter()
            .any(|it| matches!(&it.expr, CExpr::Func { name, .. } if is_aggregate(name)));
        if let CExpr::Var(name) = e {
            if let Some(i) = ret.items.iter().position(|it| it.alias.as_deref() == Some(name)) {
                return Ok(item_head_var(ret, i, has_aggr));
            }
        }
        if let Some(i) = ret.items.iter().position(|it| expr_eq(&it.expr, e)) {
            return Ok(item_head_var(ret, i, has_aggr));
        }
        bail!("ORDER BY must reference a returned column or alias in v1")
    }

    // --- schema resolution ---

    fn resolve_node(&self, n: &NodeB) -> Result<NodeMap> {
        match &n.label {
            Some(l) => self
                .schema
                .node(l)
                .cloned()
                .ok_or_else(|| miette::miette!("no schema mapping for node label `{l}`")),
            None => {
                let first = self
                    .schema
                    .nodes
                    .first()
                    .ok_or_else(|| miette::miette!("node pattern needs a label (empty schema)"))?;
                let shared = self
                    .schema
                    .nodes
                    .iter()
                    .all(|m| m.relation == first.relation && m.id_col == first.id_col);
                if !shared {
                    bail!("unlabeled node pattern requires a single shared node relation");
                }
                Ok(NodeMap {
                    label: String::new(),
                    relation: first.relation.clone(),
                    id_col: first.id_col.clone(),
                    label_col: None,
                    label_value: None,
                    filter: first.filter.clone(),
                })
            }
        }
    }

    fn resolve_edge(&self, r: &RelB) -> Result<EdgeMap> {
        match &r.rel_type {
            Some(t) => self
                .schema
                .edge(t)
                .cloned()
                .ok_or_else(|| miette::miette!("no schema mapping for relationship type `{t}`")),
            None => {
                let first = self
                    .schema
                    .edges
                    .first()
                    .ok_or_else(|| miette::miette!("relationship needs a type (empty schema)"))?;
                let shared = self.schema.edges.iter().all(|m| m.relation == first.relation);
                if !shared {
                    bail!("untyped relationship requires a single shared edge relation");
                }
                Ok(EdgeMap {
                    rel_type: String::new(),
                    relation: first.relation.clone(),
                    from_col: first.from_col.clone(),
                    to_col: first.to_col.clone(),
                    type_col: None,
                    type_value: None,
                    eid_col: first.eid_col.clone(),
                    filter: first.filter.clone(),
                })
            }
        }
    }

    // --- pattern collection ---

    fn collect(&mut self, query: &CypherQuery) -> Result<(Vec<NodeB>, Vec<RelB>)> {
        let mut nodes: Vec<NodeB> = Vec::new();
        let mut rels: Vec<RelB> = Vec::new();
        let mut rel_var_names: Vec<String> = Vec::new();
        for (ci, rc) in query.reading.iter().enumerate() {
            for pat in &rc.patterns {
                let mut prev = self.upsert_node(&mut nodes, &pat.start)?;
                for (rel, node) in &pat.rels {
                    let cur = self.upsert_node(&mut nodes, node)?;
                    let (from_v, to_v) = match rel.dir {
                        Direction::LtoR => (prev.clone(), cur.clone()),
                        Direction::RtoL => (cur.clone(), prev.clone()),
                        Direction::Undirected => {
                            bail!("undirected relationships are not yet supported in v1")
                        }
                    };
                    if let Some(rv) = &rel.var {
                        ident_ok(rv)?;
                        reserved_check(rv)?;
                        if rel_var_names.contains(rv) {
                            bail!("relationship variable `{rv}` is reused; each relationship needs a distinct variable");
                        }
                        rel_var_names.push(rv.clone());
                    }
                    rels.push(RelB {
                        clause: ci,
                        rel_type: rel.rel_type.clone(),
                        user_var: rel.var.clone(),
                        from_v,
                        to_v,
                        props: rel.props.clone(),
                    });
                    prev = cur;
                }
            }
        }
        // A name cannot be both a node and a relationship variable.
        for n in &nodes {
            if rel_var_names.contains(&n.var) {
                bail!("`{}` is used as both a node and a relationship variable", n.var);
            }
        }
        Ok((nodes, rels))
    }

    fn upsert_node(&mut self, nodes: &mut Vec<NodeB>, np: &NodePat) -> Result<String> {
        let var = match &np.var {
            Some(v) => {
                ident_ok(v)?;
                reserved_check(v)?;
                v.clone()
            }
            None => self.fresh("cyn"),
        };
        if let Some(existing) = nodes.iter_mut().find(|n| n.var == var) {
            if let Some(l) = &np.label {
                match &existing.label {
                    Some(el) if el != l => {
                        bail!("variable `{var}` has conflicting labels `{el}` and `{l}`")
                    }
                    None => existing.label = Some(l.clone()),
                    _ => {}
                }
            }
            existing.props.extend(np.props.clone());
        } else {
            nodes.push(NodeB {
                var: var.clone(),
                label: np.label.clone(),
                props: np.props.clone(),
            });
        }
        Ok(var)
    }

    /// Reject references to variables that aren't bound by a pattern, so callers
    /// get a clear error instead of an opaque engine "unbound variable".
    fn validate_bindings(
        &self,
        query: &CypherQuery,
        nodes: &[NodeB],
        rels: &[RelB],
    ) -> Result<()> {
        let node_vars: BTreeSet<String> = nodes.iter().map(|n| n.var.clone()).collect();
        let mut rel_eid: BTreeMap<String, bool> = BTreeMap::new();
        for r in rels {
            if let Some(rv) = &r.user_var {
                let em = self.resolve_edge(r)?;
                rel_eid.insert(rv.clone(), em.eid_col.is_some());
            }
        }
        let aliases: BTreeSet<String> = query
            .ret
            .items
            .iter()
            .filter_map(|it| it.alias.clone())
            .collect();

        // Property accesses (`x.k`): `x` must be a node or relationship variable.
        let mut props = Vec::new();
        for rc in &query.reading {
            if let Some(p) = &rc.where_pred {
                collect_props(p, &mut props);
            }
        }
        for it in &query.ret.items {
            collect_props(&it.expr, &mut props);
        }
        for s in &query.ret.order_by {
            collect_props(&s.expr, &mut props);
        }
        for (var, _) in &props {
            if !node_vars.contains(var) && !rel_eid.contains_key(var) {
                bail!("variable `{var}` is not defined");
            }
        }

        // Bare variable references in WHERE/RETURN.
        let mut bares = Vec::new();
        for rc in &query.reading {
            if let Some(p) = &rc.where_pred {
                collect_bare_vars(p, &mut bares);
            }
        }
        for it in &query.ret.items {
            collect_bare_vars(&it.expr, &mut bares);
        }
        for v in &bares {
            check_bare(v, &node_vars, &rel_eid, &aliases, false)?;
        }
        // ORDER BY may also reference RETURN aliases.
        let mut order_bares = Vec::new();
        for s in &query.ret.order_by {
            collect_bare_vars(&s.expr, &mut order_bares);
        }
        for v in &order_bares {
            check_bare(v, &node_vars, &rel_eid, &aliases, true)?;
        }
        Ok(())
    }

    // --- expression translation (value position: RETURN, inline props, args) ---

    fn expr(&mut self, e: &CExpr) -> Result<String> {
        Ok(match e {
            CExpr::Lit(v) => self.param(v.clone()),
            CExpr::Param(name) => {
                ident_ok(name)?;
                if name.starts_with("cphr_") {
                    bail!("parameter names starting with `cphr_` are reserved");
                }
                format!("${name}")
            }
            CExpr::Var(v) => {
                ident_ok(v)?;
                v.clone()
            }
            CExpr::Prop { var, key } => {
                ident_ok(key)?;
                self.pvar(var, key)
            }
            CExpr::List(items) => {
                let parts: Result<Vec<_>> = items.iter().map(|i| self.expr(i)).collect();
                format!("[{}]", parts?.join(", "))
            }
            CExpr::Unary { op, operand } => {
                let o = self.expr(operand)?;
                match op {
                    UnOp::Neg => format!("(-{o})"),
                    UnOp::Not => format!("(!{o})"),
                    UnOp::IsNull => format!("is_null({o})"),
                    UnOp::IsNotNull => format!("(!is_null({o}))"),
                }
            }
            CExpr::Binary { op, lhs, rhs } => {
                let l = self.expr(lhs)?;
                let r = self.expr(rhs)?;
                match op {
                    BinOp::StartsWith => format!("starts_with({l}, {r})"),
                    BinOp::EndsWith => format!("ends_with({l}, {r})"),
                    BinOp::Contains => format!("str_includes({l}, {r})"),
                    BinOp::In => format!("is_in({l}, {r})"),
                    _ => format!("({l} {} {r})", infix(op)),
                }
            }
            CExpr::Func { name, .. } => {
                bail!("function `{name}` is only supported as an aggregate in RETURN (v1)")
            }
        })
    }

    // --- WHERE translation (predicate position: null-aware three-valued logic) ---

    /// Emit a CozoScript boolean that is true iff `e` is exactly TRUE in Cypher's
    /// 3VL (null and false both yield false; a null operand never aborts).
    fn truthy(&mut self, e: &CExpr) -> Result<String> {
        Ok(match e {
            CExpr::Binary { op, lhs, rhs } => match op {
                BinOp::And => format!("({} && {})", self.truthy(lhs)?, self.truthy(rhs)?),
                BinOp::Or => format!("({} || {})", self.truthy(lhs)?, self.truthy(rhs)?),
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                    let l = self.expr(lhs)?;
                    let r = self.expr(rhs)?;
                    format!("(!is_null({l}) && !is_null({r}) && ({l} {} {r}))", infix(op))
                }
                BinOp::StartsWith | BinOp::EndsWith | BinOp::Contains => {
                    let l = self.expr(lhs)?;
                    let r = self.expr(rhs)?;
                    let f = strfn(op);
                    format!("(!is_null({l}) && !is_null({r}) && {f}({l}, {r}))")
                }
                BinOp::In => {
                    let l = self.expr(lhs)?;
                    let r = self.expr(rhs)?;
                    format!("(!is_null({l}) && is_in({l}, {r}))")
                }
                _ => {
                    let v = self.expr(e)?;
                    format!("(!is_null({v}) && {v})")
                }
            },
            CExpr::Unary { op, operand } => match op {
                UnOp::Not => self.falsy(operand)?,
                UnOp::IsNull => format!("is_null({})", self.expr(operand)?),
                UnOp::IsNotNull => format!("(!is_null({}))", self.expr(operand)?),
                UnOp::Neg => {
                    let v = self.expr(e)?;
                    format!("(!is_null({v}) && {v})")
                }
            },
            _ => {
                let v = self.expr(e)?;
                format!("(!is_null({v}) && {v})")
            }
        })
    }

    /// Emit a boolean that is true iff `e` is exactly FALSE in Cypher's 3VL.
    fn falsy(&mut self, e: &CExpr) -> Result<String> {
        Ok(match e {
            CExpr::Binary { op, lhs, rhs } => match op {
                BinOp::And => format!("({} || {})", self.falsy(lhs)?, self.falsy(rhs)?),
                BinOp::Or => format!("({} && {})", self.falsy(lhs)?, self.falsy(rhs)?),
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                    let l = self.expr(lhs)?;
                    let r = self.expr(rhs)?;
                    format!("(!is_null({l}) && !is_null({r}) && !({l} {} {r}))", infix(op))
                }
                BinOp::StartsWith | BinOp::EndsWith | BinOp::Contains => {
                    let l = self.expr(lhs)?;
                    let r = self.expr(rhs)?;
                    let f = strfn(op);
                    format!("(!is_null({l}) && !is_null({r}) && !{f}({l}, {r}))")
                }
                BinOp::In => {
                    let l = self.expr(lhs)?;
                    let r = self.expr(rhs)?;
                    format!("(!is_null({l}) && !is_in({l}, {r}))")
                }
                _ => {
                    let v = self.expr(e)?;
                    format!("(!is_null({v}) && (!{v}))")
                }
            },
            CExpr::Unary { op, operand } => match op {
                UnOp::Not => self.truthy(operand)?,
                UnOp::IsNull => format!("(!is_null({}))", self.expr(operand)?),
                UnOp::IsNotNull => format!("is_null({})", self.expr(operand)?),
                UnOp::Neg => {
                    let v = self.expr(e)?;
                    format!("(!is_null({v}) && (!{v}))")
                }
            },
            _ => {
                let v = self.expr(e)?;
                format!("(!is_null({v}) && (!{v}))")
            }
        })
    }
}

// --- free helpers ---

fn infix(op: &BinOp) -> &'static str {
    match op {
        BinOp::Or => "||",
        BinOp::And => "&&",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::Le => "<=",
        BinOp::Ge => ">=",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        _ => unreachable!("non-infix op routed to infix()"),
    }
}

fn strfn(op: &BinOp) -> &'static str {
    match op {
        BinOp::StartsWith => "starts_with",
        BinOp::EndsWith => "ends_with",
        BinOp::Contains => "str_includes",
        _ => unreachable!("non-string op routed to strfn()"),
    }
}

/// The head variable a RETURN item `i` is exposed as in the final `?` rule.
fn item_head_var(ret: &ReturnClause, i: usize, has_aggr: bool) -> String {
    if !has_aggr {
        return format!("cy_ret_{i}");
    }
    if matches!(&ret.items[i].expr, CExpr::Func { name, .. } if is_aggregate(name)) {
        format!("cy_a_{i}")
    } else {
        format!("cy_g_{i}")
    }
}

fn is_aggregate(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "avg" | "min" | "max" | "collect"
    )
}

fn cozo_aggr(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "count" => "count",
        "sum" => "sum",
        "avg" => "mean",
        "min" => "min",
        "max" => "max",
        "collect" => "collect",
        _ => unreachable!("non-aggregate routed to cozo_aggr()"),
    }
}

fn collect_props(e: &CExpr, out: &mut Vec<(String, String)>) {
    match e {
        CExpr::Prop { var, key } => push_unique_pair(out, (var.clone(), key.clone())),
        CExpr::List(items) => items.iter().for_each(|i| collect_props(i, out)),
        CExpr::Unary { operand, .. } => collect_props(operand, out),
        CExpr::Binary { lhs, rhs, .. } => {
            collect_props(lhs, out);
            collect_props(rhs, out);
        }
        CExpr::Func {
            args: FuncArgs::Exprs(es),
            ..
        } => es.iter().for_each(|i| collect_props(i, out)),
        _ => {}
    }
}

fn collect_bare_vars(e: &CExpr, out: &mut Vec<String>) {
    match e {
        CExpr::Var(v) => push_unique(out, v.clone()),
        CExpr::List(items) => items.iter().for_each(|i| collect_bare_vars(i, out)),
        CExpr::Unary { operand, .. } => collect_bare_vars(operand, out),
        CExpr::Binary { lhs, rhs, .. } => {
            collect_bare_vars(lhs, out);
            collect_bare_vars(rhs, out);
        }
        CExpr::Func {
            args: FuncArgs::Exprs(es),
            ..
        } => es.iter().for_each(|i| collect_bare_vars(i, out)),
        _ => {}
    }
}

fn check_bare(
    v: &str,
    node_vars: &BTreeSet<String>,
    rel_eid: &BTreeMap<String, bool>,
    aliases: &BTreeSet<String>,
    allow_alias: bool,
) -> Result<()> {
    if node_vars.contains(v) {
        return Ok(());
    }
    if let Some(has_eid) = rel_eid.get(v) {
        if *has_eid {
            return Ok(());
        }
        bail!("relationship `{v}` cannot be returned: its edge relation has no identity column (set eid_col)");
    }
    if allow_alias && aliases.contains(v) {
        return Ok(());
    }
    bail!("variable `{v}` is not defined")
}

fn default_col_name(e: &CExpr, i: usize) -> String {
    match e {
        CExpr::Var(v) => v.clone(),
        CExpr::Prop { var, key } => format!("{var}.{key}"),
        CExpr::Func { name, args } => match args {
            FuncArgs::Star => format!("{name}(*)"),
            FuncArgs::Exprs(_) => format!("{name}(...)"),
        },
        _ => format!("col{i}"),
    }
}

fn const_int(e: &CExpr) -> Result<i64> {
    match e {
        CExpr::Lit(DataValue::Num(n)) => n
            .get_int()
            .ok_or_else(|| miette::miette!("SKIP/LIMIT must be an integer")),
        _ => bail!("SKIP/LIMIT must be a constant integer in v1"),
    }
}

fn ident_ok(s: &str) -> Result<()> {
    let mut chars = s.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(())
    } else {
        bail!("invalid identifier `{s}` (must be alphanumeric/underscore)")
    }
}

/// Reject user variable names that collide with the translator's generated
/// namespace, preventing variable capture in the emitted CozoScript.
fn reserved_check(s: &str) -> Result<()> {
    let reserved = s == "cy_match"
        || s == "cy_agg"
        || s.starts_with("cy_ret_")
        || s.starts_with("cy_g_")
        || s.starts_with("cy_a_")
        || s.starts_with("cyp_")
        || s.starts_with("cphr_")
        || tagged_digits(s, "cyn")
        || tagged_digits(s, "cyr")
        || tagged_digits(s, "cyrp");
    if reserved {
        bail!("variable name `{s}` is reserved by the Cypher translator; please rename it");
    }
    Ok(())
}

fn reject_filter(filter: Option<&String>, which: &str) -> Result<()> {
    if filter.is_some() {
        bail!("{which}.filter is reserved and not yet implemented in v1");
    }
    Ok(())
}

fn tagged_digits(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .map(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

fn push_unique(v: &mut Vec<String>, item: String) {
    if !v.contains(&item) {
        v.push(item);
    }
}

fn push_unique_pair(v: &mut Vec<(String, String)>, item: (String, String)) {
    if !v.contains(&item) {
        v.push(item);
    }
}

/// Structural equality of two expressions (for ORDER BY / RETURN matching).
fn expr_eq(a: &CExpr, b: &CExpr) -> bool {
    match (a, b) {
        (CExpr::Var(x), CExpr::Var(y)) => x == y,
        (CExpr::Prop { var: v1, key: k1 }, CExpr::Prop { var: v2, key: k2 }) => v1 == v2 && k1 == k2,
        (CExpr::Lit(x), CExpr::Lit(y)) => x == y,
        (CExpr::Param(x), CExpr::Param(y)) => x == y,
        (CExpr::List(x), CExpr::List(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| expr_eq(a, b)),
        (CExpr::Unary { op: o1, operand: e1 }, CExpr::Unary { op: o2, operand: e2 }) => {
            o1 == o2 && expr_eq(e1, e2)
        }
        (
            CExpr::Binary { op: o1, lhs: l1, rhs: r1 },
            CExpr::Binary { op: o2, lhs: l2, rhs: r2 },
        ) => o1 == o2 && expr_eq(l1, l2) && expr_eq(r1, r2),
        (CExpr::Func { name: n1, args: a1 }, CExpr::Func { name: n2, args: a2 }) => {
            n1.eq_ignore_ascii_case(n2)
                && match (a1, a2) {
                    (FuncArgs::Star, FuncArgs::Star) => true,
                    (FuncArgs::Exprs(x), FuncArgs::Exprs(y)) => {
                        x.len() == y.len() && x.iter().zip(y).all(|(a, b)| expr_eq(a, b))
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
