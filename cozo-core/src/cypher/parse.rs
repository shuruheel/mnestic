/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Parser for the read-only Cypher subset: pest parse tree -> Cypher AST.
//! Grammar in `cypher.pest`; AST in `ast.rs`. See `docs/specs/cypher-read.md`.

use miette::{bail, miette, Result};
use pest::Parser;

use super::ast::*;
use crate::data::value::DataValue;

#[derive(pest_derive::Parser)]
#[grammar = "cypher/cypher.pest"]
struct CypherParser;

type Pair<'a> = pest::iterators::Pair<'a, Rule>;

/// Parse a read-only Cypher query into the AST. Errors carry the pest location.
pub(crate) fn parse_cypher(src: &str) -> Result<CypherQuery> {
    let mut pairs =
        CypherParser::parse(Rule::query, src).map_err(|e| miette!("Cypher parse error: {e}"))?;
    let query = pairs.next().ok_or_else(|| miette!("empty Cypher parse"))?;
    parse_query(query)
}

fn parse_query(pair: Pair<'_>) -> Result<CypherQuery> {
    let mut reading = Vec::new();
    let mut ret = None;
    for p in pair.into_inner() {
        match p.as_rule() {
            Rule::reading_clause => reading.push(parse_reading(p)?),
            Rule::return_clause => ret = Some(parse_return(p)?),
            _ => {}
        }
    }
    Ok(CypherQuery {
        reading,
        ret: ret.ok_or_else(|| miette!("Cypher query missing RETURN"))?,
    })
}

fn parse_reading(pair: Pair<'_>) -> Result<ReadingClause> {
    let mut patterns = Vec::new();
    let mut where_pred = None;
    for p in pair.into_inner() {
        match p.as_rule() {
            Rule::pattern => patterns.push(parse_pattern(p)?),
            Rule::where_clause => where_pred = Some(parse_expr(only_expr(p)?)?),
            _ => {}
        }
    }
    Ok(ReadingClause {
        patterns,
        where_pred,
    })
}

fn parse_pattern(pair: Pair<'_>) -> Result<Pattern> {
    let mut inner = pair.into_inner();
    let start = parse_node(
        inner
            .next()
            .ok_or_else(|| miette!("empty pattern"))?,
    )?;
    let mut rels = Vec::new();
    while let Some(rel_p) = inner.next() {
        let rel = parse_rel(rel_p)?;
        let node = parse_node(
            inner
                .next()
                .ok_or_else(|| miette!("relationship with no end node"))?,
        )?;
        rels.push((rel, node));
    }
    Ok(Pattern { start, rels })
}

fn parse_node(pair: Pair<'_>) -> Result<NodePat> {
    let mut var = None;
    let mut label = None;
    let mut props = Vec::new();
    for p in pair.into_inner() {
        match p.as_rule() {
            Rule::ident => var = Some(p.as_str().to_string()),
            Rule::label => label = Some(child_ident(p)?),
            Rule::prop_map => props = parse_prop_map(p)?,
            _ => {}
        }
    }
    Ok(NodePat { var, label, props })
}

fn parse_rel(pair: Pair<'_>) -> Result<RelPat> {
    let mut left = None;
    let mut right = None;
    let mut var = None;
    let mut rel_type = None;
    let mut props = Vec::new();
    for p in pair.into_inner() {
        match p.as_rule() {
            Rule::rel_left => left = Some(p.as_str().to_string()),
            Rule::rel_right => right = Some(p.as_str().to_string()),
            Rule::rel_detail => {
                for d in p.into_inner() {
                    match d.as_rule() {
                        Rule::ident => var = Some(d.as_str().to_string()),
                        Rule::rel_type => rel_type = Some(child_ident(d)?),
                        Rule::prop_map => props = parse_prop_map(d)?,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let dir = match (left.as_deref(), right.as_deref()) {
        (Some("-"), Some("->")) => Direction::LtoR,
        (Some("<-"), Some("-")) => Direction::RtoL,
        (Some("-"), Some("-")) => Direction::Undirected,
        _ => bail!("unsupported relationship direction"),
    };
    Ok(RelPat {
        var,
        rel_type,
        props,
        dir,
    })
}

fn parse_prop_map(pair: Pair<'_>) -> Result<Vec<(String, CExpr)>> {
    let mut out = Vec::new();
    for pp in pair.into_inner() {
        if pp.as_rule() == Rule::prop_pair {
            let mut it = pp.into_inner();
            let key = it
                .next()
                .ok_or_else(|| miette!("property key missing"))?
                .as_str()
                .to_string();
            let val = parse_expr(
                it.next()
                    .ok_or_else(|| miette!("property value missing"))?,
            )?;
            out.push((key, val));
        }
    }
    Ok(out)
}

fn parse_return(pair: Pair<'_>) -> Result<ReturnClause> {
    let mut distinct = false;
    let mut items = Vec::new();
    let mut order_by = Vec::new();
    let mut skip = None;
    let mut limit = None;
    for p in pair.into_inner() {
        match p.as_rule() {
            Rule::distinct => distinct = true,
            Rule::return_items => items = parse_return_items(p)?,
            Rule::order_clause => order_by = parse_order(p)?,
            Rule::skip_clause => skip = Some(parse_expr(only_expr(p)?)?),
            Rule::limit_clause => limit = Some(parse_expr(only_expr(p)?)?),
            _ => {}
        }
    }
    if items.is_empty() {
        bail!("RETURN with no items");
    }
    Ok(ReturnClause {
        distinct,
        items,
        order_by,
        skip,
        limit,
    })
}

fn parse_return_items(pair: Pair<'_>) -> Result<Vec<ReturnItem>> {
    let mut out = Vec::new();
    for ri in pair.into_inner() {
        if ri.as_rule() == Rule::return_item {
            let mut expr = None;
            let mut alias = None;
            for x in ri.into_inner() {
                match x.as_rule() {
                    Rule::expr => expr = Some(parse_expr(x)?),
                    Rule::as_alias => alias = Some(child_ident(x)?),
                    _ => {}
                }
            }
            out.push(ReturnItem {
                expr: expr.ok_or_else(|| miette!("RETURN item missing expression"))?,
                alias,
            });
        }
    }
    Ok(out)
}

fn parse_order(pair: Pair<'_>) -> Result<Vec<SortItem>> {
    let mut out = Vec::new();
    for si in pair.into_inner() {
        if si.as_rule() == Rule::sort_item {
            let mut expr = None;
            let mut descending = false;
            for x in si.into_inner() {
                match x.as_rule() {
                    Rule::expr => expr = Some(parse_expr(x)?),
                    Rule::sort_dir => {
                        descending = x.as_str().to_ascii_lowercase().starts_with("desc")
                    }
                    _ => {}
                }
            }
            out.push(SortItem {
                expr: expr.ok_or_else(|| miette!("ORDER BY term missing expression"))?,
                descending,
            });
        }
    }
    Ok(out)
}

// --- expression precedence chain ---

fn parse_expr(pair: Pair<'_>) -> Result<CExpr> {
    // pair is Rule::expr -> single or_expr child
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| miette!("empty expression"))?;
    parse_or(inner)
}

fn fold_binary<'a>(
    mut operands: impl Iterator<Item = Pair<'a>>,
    op: BinOp,
    sub: impl Fn(Pair<'a>) -> Result<CExpr>,
) -> Result<CExpr> {
    let mut acc = sub(operands.next().ok_or_else(|| miette!("missing operand"))?)?;
    for rhs in operands {
        acc = CExpr::Binary {
            op,
            lhs: Box::new(acc),
            rhs: Box::new(sub(rhs)?),
        };
    }
    Ok(acc)
}

fn parse_or(pair: Pair<'_>) -> Result<CExpr> {
    fold_binary(
        pair.into_inner().filter(|p| p.as_rule() == Rule::and_expr),
        BinOp::Or,
        parse_and,
    )
}

fn parse_and(pair: Pair<'_>) -> Result<CExpr> {
    fold_binary(
        pair.into_inner().filter(|p| p.as_rule() == Rule::not_expr),
        BinOp::And,
        parse_not,
    )
}

fn parse_not(pair: Pair<'_>) -> Result<CExpr> {
    let mut neg = false;
    let mut comp = None;
    for p in pair.into_inner() {
        match p.as_rule() {
            Rule::kw_not => neg = true,
            Rule::comparison => comp = Some(parse_comparison(p)?),
            _ => {}
        }
    }
    let e = comp.ok_or_else(|| miette!("missing comparison"))?;
    Ok(if neg {
        CExpr::Unary {
            op: UnOp::Not,
            operand: Box::new(e),
        }
    } else {
        e
    })
}

fn parse_comparison(pair: Pair<'_>) -> Result<CExpr> {
    let mut it = pair.into_inner();
    let lhs = parse_add(it.next().ok_or_else(|| miette!("missing comparison lhs"))?)?;
    match it.next() {
        Some(rhs) => parse_comp_rhs(lhs, rhs),
        None => Ok(lhs),
    }
}

fn parse_comp_rhs(lhs: CExpr, pair: Pair<'_>) -> Result<CExpr> {
    let tail = pair
        .into_inner()
        .next()
        .ok_or_else(|| miette!("empty comparison rhs"))?;
    match tail.as_rule() {
        Rule::comp_tail => {
            let mut it = tail.into_inner();
            let op_str = it
                .next()
                .ok_or_else(|| miette!("missing comparison op"))?
                .as_str()
                .to_string();
            let rhs = parse_add(it.next().ok_or_else(|| miette!("missing comparison rhs"))?)?;
            let op = match op_str.as_str() {
                "=" => BinOp::Eq,
                "<>" | "!=" => BinOp::Ne,
                "<" => BinOp::Lt,
                ">" => BinOp::Gt,
                "<=" => BinOp::Le,
                ">=" => BinOp::Ge,
                o => bail!("unknown comparison operator {o}"),
            };
            Ok(binary(op, lhs, rhs))
        }
        Rule::null_tail => {
            let is_not = tail.into_inner().any(|x| x.as_rule() == Rule::isnot_null);
            Ok(CExpr::Unary {
                op: if is_not { UnOp::IsNotNull } else { UnOp::IsNull },
                operand: Box::new(lhs),
            })
        }
        Rule::str_tail => {
            let mut it = tail.into_inner();
            let op_pair = it.next().ok_or_else(|| miette!("missing string op"))?;
            let op = match op_pair.into_inner().next().map(|x| x.as_rule()) {
                Some(Rule::starts_with) => BinOp::StartsWith,
                Some(Rule::ends_with) => BinOp::EndsWith,
                _ => BinOp::Contains,
            };
            let rhs = parse_add(it.next().ok_or_else(|| miette!("missing string rhs"))?)?;
            Ok(binary(op, lhs, rhs))
        }
        Rule::in_tail => {
            let rhs = parse_add(
                tail.into_inner()
                    .find(|x| x.as_rule() == Rule::add_expr)
                    .ok_or_else(|| miette!("missing IN rhs"))?,
            )?;
            Ok(binary(BinOp::In, lhs, rhs))
        }
        r => bail!("unexpected comparison tail {r:?}"),
    }
}

fn parse_add(pair: Pair<'_>) -> Result<CExpr> {
    let mut it = pair.into_inner();
    let mut acc = parse_mul(it.next().ok_or_else(|| miette!("missing term"))?)?;
    while let Some(op_pair) = it.next() {
        let op = if op_pair.as_str() == "+" {
            BinOp::Add
        } else {
            BinOp::Sub
        };
        let rhs = parse_mul(it.next().ok_or_else(|| miette!("missing rhs term"))?)?;
        acc = binary(op, acc, rhs);
    }
    Ok(acc)
}

fn parse_mul(pair: Pair<'_>) -> Result<CExpr> {
    let mut it = pair.into_inner();
    let mut acc = parse_unary(it.next().ok_or_else(|| miette!("missing factor"))?)?;
    while let Some(op_pair) = it.next() {
        let op = match op_pair.as_str() {
            "*" => BinOp::Mul,
            "/" => BinOp::Div,
            _ => BinOp::Mod,
        };
        let rhs = parse_unary(it.next().ok_or_else(|| miette!("missing rhs factor"))?)?;
        acc = binary(op, acc, rhs);
    }
    Ok(acc)
}

fn parse_unary(pair: Pair<'_>) -> Result<CExpr> {
    let mut neg = false;
    let mut atom = None;
    for p in pair.into_inner() {
        match p.as_rule() {
            Rule::neg_minus => neg = true,
            _ => atom = Some(parse_atom(p)?),
        }
    }
    let e = atom.ok_or_else(|| miette!("missing atom"))?;
    Ok(if neg {
        CExpr::Unary {
            op: UnOp::Neg,
            operand: Box::new(e),
        }
    } else {
        e
    })
}

fn parse_atom(pair: Pair<'_>) -> Result<CExpr> {
    match pair.as_rule() {
        Rule::number => {
            let s = pair.as_str();
            if s.contains('.') || s.contains('e') || s.contains('E') {
                let f: f64 = s.parse().map_err(|_| miette!("bad number {s}"))?;
                Ok(CExpr::Lit(DataValue::from(f)))
            } else {
                let i: i64 = s.parse().map_err(|_| miette!("bad integer {s}"))?;
                Ok(CExpr::Lit(DataValue::from(i)))
            }
        }
        Rule::string => Ok(CExpr::Lit(DataValue::Str(unquote(pair.as_str()).into()))),
        Rule::boolean => Ok(CExpr::Lit(DataValue::Bool(
            pair.as_str().eq_ignore_ascii_case("true"),
        ))),
        Rule::null_kw => Ok(CExpr::Lit(DataValue::Null)),
        Rule::param => Ok(CExpr::Param(pair.as_str()[1..].to_string())),
        Rule::var_ref => Ok(CExpr::Var(pair.as_str().to_string())),
        Rule::property => {
            let mut it = pair.into_inner();
            let var = it
                .next()
                .ok_or_else(|| miette!("property missing variable"))?
                .as_str()
                .to_string();
            let key = it
                .next()
                .ok_or_else(|| miette!("property missing key"))?
                .as_str()
                .to_string();
            Ok(CExpr::Prop { var, key })
        }
        Rule::func_call => parse_func(pair),
        Rule::grouping => parse_expr(
            pair.into_inner()
                .next()
                .ok_or_else(|| miette!("empty parentheses"))?,
        ),
        Rule::list_lit => {
            let mut items = Vec::new();
            for x in pair.into_inner() {
                if x.as_rule() == Rule::expr {
                    items.push(parse_expr(x)?);
                }
            }
            Ok(CExpr::List(items))
        }
        r => bail!("unexpected expression atom {r:?}"),
    }
}

fn parse_func(pair: Pair<'_>) -> Result<CExpr> {
    let mut it = pair.into_inner();
    let name = it
        .next()
        .ok_or_else(|| miette!("function call missing name"))?
        .as_str()
        .to_string();
    let mut args = FuncArgs::Exprs(Vec::new());
    if let Some(fa) = it.next() {
        if fa.as_rule() == Rule::func_args {
            let mut star = false;
            let mut exprs = Vec::new();
            for x in fa.into_inner() {
                match x.as_rule() {
                    Rule::star_arg => star = true,
                    Rule::expr => exprs.push(parse_expr(x)?),
                    _ => {}
                }
            }
            args = if star {
                FuncArgs::Star
            } else {
                FuncArgs::Exprs(exprs)
            };
        }
    }
    Ok(CExpr::Func { name, args })
}

// --- helpers ---

fn binary(op: BinOp, lhs: CExpr, rhs: CExpr) -> CExpr {
    CExpr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

/// Extract the single `expr` child of a wrapper rule (where/skip/limit clauses).
fn only_expr(pair: Pair<'_>) -> Result<Pair<'_>> {
    pair.into_inner()
        .find(|x| x.as_rule() == Rule::expr)
        .ok_or_else(|| miette!("clause missing expression"))
}

/// Extract the single `ident` child of a wrapper rule (label/rel_type/as_alias).
fn child_ident(pair: Pair<'_>) -> Result<String> {
    pair.into_inner()
        .find(|x| x.as_rule() == Rule::ident)
        .map(|x| x.as_str().to_string())
        .ok_or_else(|| miette!("expected identifier"))
}

fn unquote(s: &str) -> String {
    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('b') => out.push('\u{0008}'),
                Some('f') => out.push('\u{000C}'),
                Some('0') => out.push('\0'),
                Some('u') => {
                    // \uXXXX — four hex digits (matches the CozoScript char rule).
                    let hex: String = chars.by_ref().take(4).collect();
                    match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        Some(ch) => out.push(ch),
                        None => {
                            out.push('\u{FFFD}');
                        }
                    }
                }
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_query() {
        let q = parse_cypher(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 \
             RETURN b.name AS name, count(*) AS c ORDER BY c DESC LIMIT 10",
        )
        .unwrap();
        assert_eq!(q.reading.len(), 1);
        let r = &q.reading[0];
        assert_eq!(r.patterns.len(), 1);
        let p = &r.patterns[0];
        assert_eq!(p.start.label.as_deref(), Some("Person"));
        assert_eq!(p.rels.len(), 1);
        let (rel, end) = &p.rels[0];
        assert_eq!(rel.dir, Direction::LtoR);
        assert_eq!(rel.rel_type.as_deref(), Some("KNOWS"));
        assert_eq!(end.label.as_deref(), Some("Person"));
        assert!(r.where_pred.is_some());
        assert_eq!(q.ret.items.len(), 2);
        assert_eq!(q.ret.items[0].alias.as_deref(), Some("name"));
        assert_eq!(q.ret.order_by.len(), 1);
        assert!(q.ret.order_by[0].descending);
        assert!(!q.ret.distinct);
        assert!(q.ret.limit.is_some());
    }

    #[test]
    fn parses_minimal() {
        let q = parse_cypher("MATCH (a) RETURN a").unwrap();
        assert_eq!(q.reading[0].patterns[0].start.var.as_deref(), Some("a"));
        assert!(q.reading[0].patterns[0].start.label.is_none());
        assert_eq!(q.ret.items.len(), 1);
        assert!(matches!(q.ret.items[0].expr, CExpr::Var(ref v) if v == "a"));
    }

    #[test]
    fn parses_inline_props_and_distinct() {
        let q = parse_cypher("MATCH (a:P {name: 'Bob', age: 30}) RETURN DISTINCT a.x").unwrap();
        let start = &q.reading[0].patterns[0].start;
        assert_eq!(start.props.len(), 2);
        assert_eq!(start.props[0].0, "name");
        assert!(matches!(&start.props[0].1, CExpr::Lit(DataValue::Str(s)) if s == "Bob"));
        assert!(q.ret.distinct);
    }

    #[test]
    fn parses_relationship_directions() {
        assert_eq!(
            parse_cypher("MATCH (a)<-[:R]-(b) RETURN a").unwrap().reading[0].patterns[0].rels[0]
                .0
                .dir,
            Direction::RtoL
        );
        assert_eq!(
            parse_cypher("MATCH (a)-[r]-(b) RETURN r").unwrap().reading[0].patterns[0].rels[0]
                .0
                .dir,
            Direction::Undirected
        );
    }

    #[test]
    fn keyword_prefixed_identifiers_are_not_literals() {
        // `nullable` must parse as a property key, not the `null` literal;
        // `trueish` as a var, not `true` — the openCypher analogue of cozo #281.
        let q = parse_cypher("MATCH (a) WHERE a.nullable = true RETURN a.trueish").unwrap();
        assert!(q.reading[0].where_pred.is_some());
        assert!(matches!(&q.ret.items[0].expr, CExpr::Prop { key, .. } if key == "trueish"));
    }

    #[test]
    fn parses_where_predicates() {
        for src in [
            "MATCH (a) WHERE a.name IS NULL RETURN a",
            "MATCH (a) WHERE a.name IS NOT NULL RETURN a",
            "MATCH (a) WHERE a.name STARTS WITH 'B' RETURN a",
            "MATCH (a) WHERE a.name CONTAINS 'o' RETURN a",
            "MATCH (a) WHERE a.age IN [1, 2, 3] RETURN a",
            "MATCH (a) WHERE a.age > 30 AND a.age < 40 OR NOT a.flag RETURN a",
        ] {
            assert!(parse_cypher(src).is_ok(), "should parse: {src}");
        }
    }

    #[test]
    fn rejects_unsupported_and_malformed() {
        // No RETURN.
        assert!(parse_cypher("MATCH (a)").is_err());
        // RETURN-only (no MATCH) — a documented v1 limitation.
        assert!(parse_cypher("RETURN 1").is_err());
        // Write clause — out of scope, should not parse.
        assert!(parse_cypher("CREATE (a:Person) RETURN a").is_err());
        // Dangling relationship.
        assert!(parse_cypher("MATCH (a)-[:R]-> RETURN a").is_err());
    }

    #[test]
    fn parses_multiple_match_clauses() {
        let q = parse_cypher("MATCH (a:P) MATCH (b:Q) WHERE b.x = 1 RETURN a, b").unwrap();
        assert_eq!(q.reading.len(), 2);
        assert!(q.reading[0].where_pred.is_none());
        assert!(q.reading[1].where_pred.is_some());
    }
}
