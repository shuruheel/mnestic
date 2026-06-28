/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! AST for the read-only Cypher subset (mnestic fork). Produced by `parse.rs`,
//! consumed by the translator (`translate.rs`, later step). See
//! `docs/specs/cypher-read.md`.

use crate::data::value::DataValue;

/// A parsed read-only Cypher query: one or more reading clauses then a RETURN.
#[derive(Debug, Clone)]
pub(crate) struct CypherQuery {
    pub reading: Vec<ReadingClause>,
    pub ret: ReturnClause,
}

/// `MATCH <patterns> [WHERE <pred>]`.
#[derive(Debug, Clone)]
pub(crate) struct ReadingClause {
    pub patterns: Vec<Pattern>,
    pub where_pred: Option<CExpr>,
}

/// A path pattern: a start node then zero or more `(rel, node)` hops.
#[derive(Debug, Clone)]
pub(crate) struct Pattern {
    pub start: NodePat,
    pub rels: Vec<(RelPat, NodePat)>,
}

/// `(var:Label {props})` — every part optional except the surrounding parens.
#[derive(Debug, Clone)]
pub(crate) struct NodePat {
    pub var: Option<String>,
    pub label: Option<String>,
    pub props: Vec<(String, CExpr)>,
}

/// `-[var:TYPE {props}]->` and its direction.
#[derive(Debug, Clone)]
pub(crate) struct RelPat {
    pub var: Option<String>,
    pub rel_type: Option<String>,
    pub props: Vec<(String, CExpr)>,
    pub dir: Direction,
}

/// Relationship direction. `LtoR` = `-->`, `RtoL` = `<--`, `Undirected` = `--`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    LtoR,
    RtoL,
    Undirected,
}

/// `RETURN [DISTINCT] <items> [ORDER BY ...] [SKIP n] [LIMIT m]`.
#[derive(Debug, Clone)]
pub(crate) struct ReturnClause {
    pub distinct: bool,
    pub items: Vec<ReturnItem>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<CExpr>,
    pub limit: Option<CExpr>,
}

/// A `RETURN` projection: an expression with an optional `AS` alias.
#[derive(Debug, Clone)]
pub(crate) struct ReturnItem {
    pub expr: CExpr,
    pub alias: Option<String>,
}

/// An `ORDER BY` term.
#[derive(Debug, Clone)]
pub(crate) struct SortItem {
    pub expr: CExpr,
    pub descending: bool,
}

/// A Cypher expression (WHERE predicates, RETURN items, inline property values).
#[derive(Debug, Clone)]
pub(crate) enum CExpr {
    /// A literal value.
    Lit(DataValue),
    /// A query parameter `$name`.
    Param(String),
    /// A bare variable reference (a bound node/relationship variable).
    Var(String),
    /// Property access `var.key`.
    Prop { var: String, key: String },
    /// A list literal `[a, b, ...]`.
    List(Vec<CExpr>),
    /// A unary operation.
    Unary { op: UnOp, operand: Box<CExpr> },
    /// A binary operation.
    Binary {
        op: BinOp,
        lhs: Box<CExpr>,
        rhs: Box<CExpr>,
    },
    /// A function/aggregate call, e.g. `count(*)`, `sum(x)`.
    Func { name: String, args: FuncArgs },
}

/// Arguments of a function call.
#[derive(Debug, Clone)]
pub(crate) enum FuncArgs {
    /// `count(*)`.
    Star,
    /// Positional expression arguments.
    Exprs(Vec<CExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnOp {
    Neg,
    Not,
    IsNull,
    IsNotNull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    StartsWith,
    EndsWith,
    Contains,
    In,
}
