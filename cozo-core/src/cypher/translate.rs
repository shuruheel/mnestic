/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Translator: Cypher AST + property-graph schema -> CozoScript string + params.
//!
//! The output runs through the normal read-only query path (later step). Literals
//! become params (`$cphr_N`) so nothing user-supplied is interpolated; every
//! identifier from the schema is validated. Bag semantics are preserved via a
//! hidden binding-key in the head (set semantics over distinct bindings = the
//! Cypher bag); `RETURN DISTINCT` and aggregation collapse as Cypher specifies.
//! See `docs/specs/cypher-read.md` §2–§6.
//!
//! v1 scope (errors, not silent gaps, for the rest): directed relationships,
//! labels (or unlabeled over a single shared node relation), WHERE, RETURN
//! (DISTINCT / bag / aggregates), inline-property filters, edge-isomorphism,
//! ORDER BY / SKIP / LIMIT over projected columns. Deferred: undirected
//! relationships, the schema `filter` field, variable-length paths, OPTIONAL
//! MATCH, WITH.

use std::collections::BTreeMap;
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

struct Translator<'s> {
    schema: &'s CypherGraphSchema,
    params: BTreeMap<String, DataValue>,
    pcount: usize,
    anon: usize,
}

impl<'s> Translator<'s> {
    fn new(schema: &'s CypherGraphSchema) -> Self {
        Translator {
            schema,
            params: BTreeMap::new(),
            pcount: 0,
            anon: 0,
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

    fn run(&mut self, query: &CypherQuery) -> Result<CypherScript> {
        let (nodes, rels) = self.collect(query)?;

        // Properties referenced anywhere must be bound in the node access; those
        // used in RETURN/ORDER must also be exposed in the cy_match head.
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
            body.push(self.node_atom(n, &all_props)?);
        }
        // Edge atoms + identity tracking for edge-isomorphism.
        let mut rel_ids: Vec<(usize, String, Option<String>, String, String)> = Vec::new();
        for r in &rels {
            let (atom, edge_relation, eid_var) = self.rel_atom(r, &all_props)?;
            body.push(atom);
            rel_ids.push((r.clause, edge_relation, eid_var, r.from_v.clone(), r.to_v.clone()));
        }
        // WHERE filters.
        for rc in &query.reading {
            if let Some(p) = &rc.where_pred {
                body.push(self.expr(p)?);
            }
        }
        // Edge-isomorphism: same clause + same edge relation must be distinct.
        for i in 0..rel_ids.len() {
            for j in (i + 1)..rel_ids.len() {
                let (ci, ri, eid_i, fi, ti) = &rel_ids[i];
                let (cj, rj, eid_j, fj, tj) = &rel_ids[j];
                if ci != cj || ri != rj {
                    continue;
                }
                match (eid_i, eid_j) {
                    (Some(a), Some(b)) => body.push(format!("{a} != {b}")),
                    _ => body.push(format!("({fi} != {fj} or {ti} != {tj})")),
                }
            }
        }

        // cy_match head: node vars, then rel eid vars, then head-prop vars.
        let mut match_head: Vec<String> = nodes.iter().map(|n| n.var.clone()).collect();
        for (_, _, eid, _, _) in &rel_ids {
            if let Some(e) = eid {
                push_unique(&mut match_head, e.clone());
            }
        }
        for (var, key) in &head_props {
            push_unique(&mut match_head, prop_var(var, key));
        }

        let mut script = String::new();
        writeln!(
            script,
            "cy_match[{}] := {}",
            match_head.join(", "),
            body.join(", ")
        )
        .unwrap();

        // --- projection / aggregation ---
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
            .map(|(i, it)| it.alias.clone().unwrap_or_else(|| default_col_name(&it.expr, i)))
            .collect();

        if has_aggr {
            self.emit_aggregate(&mut script, query, &match_head, &nodes)?;
        } else {
            self.emit_projection(&mut script, query, &match_head)?;
        }

        self.emit_epilogue(&mut script, &query.ret, &out_columns)?;

        Ok(CypherScript {
            script,
            params: std::mem::take(&mut self.params),
            out_columns,
        })
    }

    /// Build a node relation access atom, binding id, discriminator, and props.
    fn node_atom(&mut self, n: &NodeB, all_props: &[(String, String)]) -> Result<String> {
        let nm = self.resolve_node(n)?;
        reject_filter(nm.filter.as_ref(), "NodeMap")?;
        ident_ok(&nm.relation)?;
        ident_ok(&nm.id_col)?;
        let mut args = vec![format!("{}: {}", nm.id_col, n.var)];

        if let Some(lc) = &nm.label_col {
            ident_ok(lc)?;
            let v = nm
                .label_value
                .clone()
                .unwrap_or_else(|| DataValue::Str(n.label.clone().unwrap_or_default().into()));
            args.push(format!("{}: {}", lc, self.param(v)));
        }

        // Columns this node touches: referenced props (bind) and inline props (filter).
        let referenced: Vec<&str> = all_props
            .iter()
            .filter(|(v, _)| *v == n.var)
            .map(|(_, k)| k.as_str())
            .collect();
        let mut extra_eq: Vec<String> = Vec::new();
        let mut seen_cols: Vec<String> = Vec::new();
        for key in &referenced {
            ident_ok(key)?;
            args.push(format!("{}: {}", key, prop_var(&n.var, key)));
            seen_cols.push((*key).to_string());
            if let Some((_, val)) = n.props.iter().find(|(k, _)| k == key) {
                let pv = self.expr(val)?;
                extra_eq.push(format!("{} == {}", prop_var(&n.var, key), pv));
            }
        }
        for (key, val) in &n.props {
            if seen_cols.iter().any(|c| c == key) {
                continue;
            }
            ident_ok(key)?;
            let pv = self.expr(val)?;
            args.push(format!("{}: {}", key, pv));
        }

        let mut atom = format!("*{}{{{}}}", nm.relation, args.join(", "));
        for eq in extra_eq {
            atom.push_str(", ");
            atom.push_str(&eq);
        }
        Ok(atom)
    }

    /// Build an edge relation access atom; returns (atom, edge_relation, eid_var).
    fn rel_atom(
        &mut self,
        r: &RelB,
        all_props: &[(String, String)],
    ) -> Result<(String, String, Option<String>)> {
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

        if let Some(tc) = &em.type_col {
            ident_ok(tc)?;
            let v = em
                .type_value
                .clone()
                .unwrap_or_else(|| DataValue::Str(r.rel_type.clone().unwrap_or_default().into()));
            args.push(format!("{}: {}", tc, self.param(v)));
        }

        // Inline relationship property filters, and any referenced via the rel var.
        let rel_var = r.user_var.as_deref();
        let mut seen_cols: Vec<String> = Vec::new();
        if let Some(rv) = rel_var {
            let referenced: Vec<&str> = all_props
                .iter()
                .filter(|(v, _)| v == rv)
                .map(|(_, k)| k.as_str())
                .collect();
            for key in referenced {
                ident_ok(key)?;
                args.push(format!("{}: {}", key, prop_var(rv, key)));
                seen_cols.push(key.to_string());
            }
        }
        for (key, val) in &r.props {
            if seen_cols.iter().any(|c| c == key) {
                continue;
            }
            ident_ok(key)?;
            let pv = self.expr(val)?;
            args.push(format!("{}: {}", key, pv));
        }

        Ok((
            format!("*{}{{{}}}", em.relation, args.join(", ")),
            em.relation.clone(),
            eid_var,
        ))
    }

    fn emit_projection(
        &mut self,
        script: &mut String,
        query: &CypherQuery,
        match_head: &[String],
    ) -> Result<()> {
        let mut ret_vars = Vec::new();
        let mut unifies = Vec::new();
        for (i, item) in query.ret.items.iter().enumerate() {
            let var = format!("cy_ret_{i}");
            unifies.push(format!("{var} = {}", self.expr(&item.expr)?));
            ret_vars.push(var);
        }

        // Bag mode: append the binding key (node vars + rel eids) as hidden
        // columns so duplicate projected rows survive. DISTINCT omits them.
        let mut head = ret_vars.clone();
        if !query.ret.distinct {
            for v in binding_key(match_head, query) {
                push_unique(&mut head, v);
            }
        }

        let mut body = vec![format!("cy_match[{}]", match_head.join(", "))];
        body.extend(unifies);
        writeln!(script, "?[{}] := {}", head.join(", "), body.join(", ")).unwrap();
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

        // cy_agg head: group columns (computed) then aggregate head-args.
        let mut agg_head: Vec<String> = Vec::new();
        let mut group_unifies: Vec<String> = Vec::new();
        let mut col_for_item: Vec<String> = Vec::new(); // var name to read each item back as
        for (i, item) in query.ret.items.iter().enumerate() {
            if let CExpr::Func { name, args } = &item.expr {
                if is_aggregate(name) {
                    let arg = self.aggregate_arg(args, &anchor)?;
                    let col = format!("cy_a_{i}");
                    agg_head.push(format!("{}({arg})", cozo_aggr(name)));
                    col_for_item.push(col);
                    continue;
                }
            }
            let gv = format!("cy_g_{i}");
            group_unifies.push(format!("{gv} = {}", self.expr(&item.expr)?));
            agg_head.push(gv.clone());
            col_for_item.push(gv);
        }

        let mut agg_body = vec![format!("cy_match[{}]", match_head.join(", "))];
        agg_body.extend(group_unifies);
        writeln!(
            script,
            "cy_agg[{}] := {}",
            agg_head.join(", "),
            agg_body.join(", ")
        )
        .unwrap();

        // Final rule names cy_agg's columns and reorders to RETURN order.
        // cy_agg's positional columns are exactly col_for_item order.
        let bind_names: Vec<String> = col_for_item.clone();
        let head: Vec<String> = col_for_item.clone();
        writeln!(
            script,
            "?[{}] := cy_agg[{}]",
            head.join(", "),
            bind_names.join(", ")
        )
        .unwrap();
        Ok(())
    }

    fn aggregate_arg(&mut self, args: &FuncArgs, anchor: &str) -> Result<String> {
        match args {
            FuncArgs::Star => Ok(anchor.to_string()),
            FuncArgs::Exprs(es) => {
                if es.len() != 1 {
                    bail!("aggregate functions take exactly one argument in v1");
                }
                match &es[0] {
                    CExpr::Var(v) => {
                        ident_ok(v)?;
                        Ok(v.clone())
                    }
                    CExpr::Prop { var, key } => Ok(prop_var(var, key)),
                    _ => bail!("aggregate argument must be a variable or property in v1"),
                }
            }
        }
    }

    fn emit_epilogue(
        &mut self,
        script: &mut String,
        ret: &ReturnClause,
        out_columns: &[String],
    ) -> Result<()> {
        if !ret.order_by.is_empty() {
            let mut parts = Vec::new();
            for s in &ret.order_by {
                let col = self.order_var(&s.expr, ret, out_columns)?;
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
    fn order_var(
        &self,
        e: &CExpr,
        ret: &ReturnClause,
        _out_columns: &[String],
    ) -> Result<String> {
        let has_aggr = ret
            .items
            .iter()
            .any(|it| matches!(&it.expr, CExpr::Func { name, .. } if is_aggregate(name)));
        // Match by alias.
        if let CExpr::Var(name) = e {
            if let Some(i) = ret.items.iter().position(|it| it.alias.as_deref() == Some(name)) {
                return Ok(item_head_var(ret, i, has_aggr));
            }
        }
        // Match by identical projected expression.
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
                // Allowed only when all node mappings share one relation+id_col.
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
                let shared = self
                    .schema
                    .edges
                    .iter()
                    .all(|m| m.relation == first.relation);
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
        Ok((nodes, rels))
    }

    fn upsert_node(&mut self, nodes: &mut Vec<NodeB>, np: &NodePat) -> Result<String> {
        let var = match &np.var {
            Some(v) => {
                ident_ok(v)?;
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

    // --- expression translation ---

    fn expr(&mut self, e: &CExpr) -> Result<String> {
        Ok(match e {
            CExpr::Lit(v) => self.param(v.clone()),
            CExpr::Param(name) => {
                ident_ok(name)?;
                format!("${name}")
            }
            CExpr::Var(v) => {
                ident_ok(v)?;
                v.clone()
            }
            CExpr::Prop { var, key } => {
                ident_ok(key)?;
                prop_var(var, key)
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

fn prop_var(var: &str, key: &str) -> String {
    format!("cyp_{var}_{key}")
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

/// The binding-key columns (node vars + rel eid vars) within a cy_match head.
fn binding_key(match_head: &[String], _query: &CypherQuery) -> Vec<String> {
    match_head
        .iter()
        .filter(|v| !v.starts_with("cyp_"))
        .cloned()
        .collect()
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

fn reject_filter(filter: Option<&String>, which: &str) -> Result<()> {
    if filter.is_some() {
        bail!("{which}.filter is reserved and not yet implemented in v1");
    }
    Ok(())
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
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::parse::parse_cypher;
    use crate::{DbInstance, ScriptMutability};

    fn tr(q: &str, schema: &CypherGraphSchema) -> CypherScript {
        cypher_to_script(&parse_cypher(q).unwrap(), schema).unwrap()
    }

    fn nm(label: &str, relation: &str, id_col: &str, label_col: Option<&str>) -> NodeMap {
        NodeMap {
            label: label.into(),
            relation: relation.into(),
            id_col: id_col.into(),
            label_col: label_col.map(|s| s.into()),
            label_value: None,
            filter: None,
        }
    }

    fn relation_per_label() -> CypherGraphSchema {
        CypherGraphSchema {
            nodes: vec![nm("Person", "person", "id", None)],
            edges: vec![EdgeMap {
                rel_type: "KNOWS".into(),
                relation: "knows".into(),
                from_col: "fr".into(),
                to_col: "to".into(),
                type_col: None,
                type_value: None,
                eid_col: None,
                filter: None,
            }],
        }
    }

    fn shared_relation() -> CypherGraphSchema {
        CypherGraphSchema {
            nodes: vec![nm("Person", "node", "uid", Some("node_type"))],
            edges: vec![EdgeMap {
                rel_type: "KNOWS".into(),
                relation: "edge".into(),
                from_col: "from_uid".into(),
                to_col: "to_uid".into(),
                type_col: Some("edge_type".into()),
                type_value: None,
                eid_col: Some("uid".into()),
                filter: None,
            }],
        }
    }

    /// Project each result row to the user-visible columns and return as strings
    /// for order-independent comparison.
    fn run(db: &DbInstance, cs: &CypherScript) -> Vec<Vec<DataValue>> {
        let out = db
            .run_script(&cs.script, cs.params.clone(), ScriptMutability::Immutable)
            .unwrap_or_else(|e| panic!("script failed:\n{}\nerror: {e}", cs.script));
        let n = cs.out_columns.len();
        out.rows.into_iter().map(|r| r[..n].to_vec()).collect()
    }

    #[test]
    fn golden_relation_per_label() {
        let cs = tr(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 RETURN b.name AS name ORDER BY name",
            &relation_per_label(),
        );
        assert!(cs.script.contains("*person{id: a, age: cyp_a_age}"), "{}", cs.script);
        assert!(cs.script.contains("*person{id: b, name: cyp_b_name}"), "{}", cs.script);
        assert!(cs.script.contains("*knows{fr: a, to: b}"), "{}", cs.script);
        assert!(cs.script.contains(":order +cy_ret_0"), "{}", cs.script);
        assert_eq!(cs.out_columns, vec!["name"]);
        assert_eq!(cs.params.len(), 1); // the literal 30
    }

    #[test]
    fn golden_shared_relation_with_eid_and_iso() {
        let cs = tr(
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN c.name",
            &shared_relation(),
        );
        // discriminators are params; reified edge bound by uid.
        assert!(cs.script.contains("node_type: $cphr"), "{}", cs.script);
        assert!(cs.script.contains("edge_type: $cphr"), "{}", cs.script);
        assert!(cs.script.contains("uid:"), "{}", cs.script);
        // edge-isomorphism over the two reified edge ids.
        assert!(cs.script.contains(" != "), "missing iso disequality:\n{}", cs.script);
    }

    #[test]
    fn exec_relation_per_label_where_order() {
        let db = DbInstance::default();
        db.run_default(":create person {id => name, age}").unwrap();
        db.run_default(":create knows {fr, to}").unwrap();
        db.run_default(
            "?[id, name, age] <- [[1,'Alice',30],[2,'Bob',40],[3,'Carol',35]] :put person {id => name, age}",
        )
        .unwrap();
        db.run_default("?[fr, to] <- [[1,2],[1,3],[2,3]] :put knows {fr, to}").unwrap();

        let cs = tr(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 RETURN b.name AS name ORDER BY name",
            &relation_per_label(),
        );
        assert_eq!(run(&db, &cs), vec![vec![DataValue::from("Carol")]]);
    }

    #[test]
    fn exec_aggregate_count() {
        let db = DbInstance::default();
        db.run_default(":create person {id => name}").unwrap();
        db.run_default(":create knows {fr, to}").unwrap();
        db.run_default("?[id, name] <- [[1,'Alice'],[2,'Bob'],[3,'Carol']] :put person {id => name}")
            .unwrap();
        db.run_default("?[fr, to] <- [[1,2],[1,3],[2,3]] :put knows {fr, to}").unwrap();

        let cs = tr(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS who, count(*) AS c ORDER BY who",
            &relation_per_label(),
        );
        assert_eq!(
            run(&db, &cs),
            vec![
                vec![DataValue::from("Alice"), DataValue::from(2i64)],
                vec![DataValue::from("Bob"), DataValue::from(1i64)],
            ]
        );
    }

    #[test]
    fn exec_bag_vs_distinct() {
        let db = DbInstance::default();
        db.run_default(":create person {id => name}").unwrap();
        db.run_default(":create knows {fr, to}").unwrap();
        db.run_default("?[id, name] <- [[1,'Alice'],[2,'Bob'],[3,'Carol']] :put person {id => name}")
            .unwrap();
        db.run_default("?[fr, to] <- [[1,2],[1,3],[2,3]] :put knows {fr, to}").unwrap();

        let schema = relation_per_label();
        // bag: a.name once per edge → Alice, Alice, Bob (3 rows).
        let bag = tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name", &schema);
        assert_eq!(run(&db, &bag).len(), 3);
        // distinct: Alice, Bob (2 rows).
        let dis = tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN DISTINCT a.name", &schema);
        assert_eq!(run(&db, &dis).len(), 2);
    }

    #[test]
    fn exec_shared_relation_mindgraph_style() {
        let db = DbInstance::default();
        db.run_default(":create node {uid => node_type, name}").unwrap();
        db.run_default(":create edge {uid => from_uid, to_uid, edge_type}").unwrap();
        db.run_default(
            "?[uid, node_type, name] <- [['n1','Person','Alice'],['n2','Person','Bob']] :put node {uid => node_type, name}",
        )
        .unwrap();
        db.run_default(
            "?[uid, from_uid, to_uid, edge_type] <- [['e1','n1','n2','KNOWS']] :put edge {uid => from_uid, to_uid, edge_type}",
        )
        .unwrap();

        let cs = tr(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS name",
            &shared_relation(),
        );
        assert_eq!(run(&db, &cs), vec![vec![DataValue::from("Bob")]]);
    }

    #[test]
    fn run_cypher_entry_projects_and_names_columns() {
        let db = DbInstance::default();
        db.run_default(":create person {id => name, age}").unwrap();
        db.run_default(":create knows {fr, to}").unwrap();
        db.run_default(
            "?[id, name, age] <- [[1,'Alice',30],[2,'Bob',40],[3,'Carol',35]] :put person {id => name, age}",
        )
        .unwrap();
        db.run_default("?[fr, to] <- [[1,2],[1,3],[2,3]] :put knows {fr, to}").unwrap();

        let out = db
            .run_cypher(
                "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 RETURN b.name AS name ORDER BY name",
                &relation_per_label(),
                BTreeMap::new(),
            )
            .unwrap();
        // Headers are the RETURN columns; hidden binding-key columns are stripped.
        assert_eq!(out.headers, vec!["name"]);
        assert_eq!(out.rows, vec![vec![DataValue::from("Carol")]]);
    }

    #[test]
    fn run_cypher_passes_user_params() {
        let db = DbInstance::default();
        db.run_default(":create person {id => name, age}").unwrap();
        db.run_default(
            "?[id, name, age] <- [[1,'Alice',30],[2,'Bob',40],[3,'Carol',35]] :put person {id => name, age}",
        )
        .unwrap();

        let mut params = BTreeMap::new();
        params.insert("minAge".to_string(), DataValue::from(30i64));
        let out = db
            .run_cypher(
                "MATCH (a:Person) WHERE a.age > $minAge RETURN a.name AS name ORDER BY name",
                &relation_per_label(),
                params,
            )
            .unwrap();
        assert_eq!(out.headers, vec!["name"]);
        assert_eq!(
            out.rows,
            vec![vec![DataValue::from("Bob")], vec![DataValue::from("Carol")]]
        );
    }

    #[test]
    fn cypher_to_script_is_inspectable() {
        let db = DbInstance::default();
        let (script, params) = db
            .cypher_to_script("MATCH (a:Person) WHERE a.age > 30 RETURN a.name", &relation_per_label())
            .unwrap();
        assert!(script.contains("*person{"), "{script}");
        assert_eq!(params.len(), 1); // the literal 30
    }

    #[test]
    fn deferred_features_error_clearly() {
        let s = relation_per_label();
        // Undirected relationships are deferred.
        assert!(cypher_to_script(&parse_cypher("MATCH (a:Person)-[:KNOWS]-(b:Person) RETURN a").unwrap(), &s).is_err());
        // Unknown label.
        assert!(cypher_to_script(&parse_cypher("MATCH (a:Ghost) RETURN a").unwrap(), &s).is_err());
        // Schema filter is reserved.
        let mut s2 = relation_per_label();
        s2.nodes[0].filter = Some("age > 0".into());
        assert!(cypher_to_script(&parse_cypher("MATCH (a:Person) RETURN a").unwrap(), &s2).is_err());
    }
}
