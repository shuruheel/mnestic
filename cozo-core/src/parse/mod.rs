/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! AST for Cozo scripts, for generating Cozo scripts programmatically.
//!
//! NOTE! This is unstable, the AST structure and method signatures may change in any release. Use at your own risk.

use std::cmp::{max, min};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use either::{Either, Left};
use miette::{bail, Diagnostic, IntoDiagnostic, Result};
use pest::error::InputLocation;
use pest::Parser;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::program::InputProgram;
use crate::data::relation::NullableColType;
use crate::data::value::{DataValue, ValidityTs};
use crate::parse::expr::build_expr;
use crate::parse::imperative::parse_imperative_block;
use crate::parse::query::parse_query;
use crate::parse::schema::parse_nullable_type;
use crate::parse::sys::{parse_sys, SysOp};
use crate::{Expr, FixedRule};

pub(crate) mod expr;
pub(crate) mod fts;
pub(crate) mod imperative;
pub(crate) mod query;
pub(crate) mod schema;
pub(crate) mod sys;

#[derive(pest_derive::Parser)]
#[grammar = "cozoscript.pest"]
pub(crate) struct CozoScriptParser;

pub(crate) type Pair<'a> = pest::iterators::Pair<'a, Rule>;
pub(crate) type Pairs<'a> = pest::iterators::Pairs<'a, Rule>;

/// This represents a full Cozo script, as you'd pass to `run_script`.
#[derive(Debug)]
pub enum CozoScript {
    #[allow(missing_docs)]
    Single(InputProgram),
    #[allow(missing_docs)]
    Imperative(ImperativeProgram),
    #[allow(missing_docs)]
    Sys(SysOp),
}

#[allow(missing_docs)]
#[derive(Debug)]
pub struct ImperativeStmtClause {
    pub prog: InputProgram,
    pub store_as: Option<SmartString<LazyCompact>>,
}

#[allow(missing_docs)]
#[derive(Debug)]
pub struct ImperativeSysop {
    pub sysop: SysOp,
    pub store_as: Option<SmartString<LazyCompact>>,
}

#[allow(missing_docs)]
#[derive(Debug)]
pub enum ImperativeStmt {
    Break {
        target: Option<SmartString<LazyCompact>>,
        span: SourceSpan,
    },
    Continue {
        target: Option<SmartString<LazyCompact>>,
        span: SourceSpan,
    },
    Return {
        returns: Vec<Either<ImperativeStmtClause, SmartString<LazyCompact>>>,
    },
    Program {
        prog: ImperativeStmtClause,
    },
    SysOp {
        sysop: ImperativeSysop,
    },
    IgnoreErrorProgram {
        prog: ImperativeStmtClause,
    },
    If {
        condition: ImperativeCondition,
        then_branch: ImperativeProgram,
        else_branch: ImperativeProgram,
        negated: bool,
    },
    Loop {
        label: Option<SmartString<LazyCompact>>,
        body: ImperativeProgram,
    },
    TempSwap {
        left: SmartString<LazyCompact>,
        right: SmartString<LazyCompact>,
        // span: SourceSpan,
    },
    TempDebug {
        temp: SmartString<LazyCompact>,
    },
}

pub(crate) type ImperativeCondition = Either<SmartString<LazyCompact>, ImperativeStmtClause>;

/// This is a [chained query](https://docs.cozodb.org/en/latest/stored.html#chaining-queries),
/// a series of `{}` queries possibly with imperative directives like `%if` and `%loop`.
pub type ImperativeProgram = Vec<ImperativeStmt>;

impl ImperativeStmt {
    pub(crate) fn needs_write_locks(&self, collector: &mut BTreeSet<SmartString<LazyCompact>>) {
        match self {
            ImperativeStmt::Program { prog, .. }
            | ImperativeStmt::IgnoreErrorProgram { prog, .. } => {
                if let Some(name) = prog.prog.needs_write_lock() {
                    collector.insert(name);
                }
            }
            ImperativeStmt::Return { returns, .. } => {
                for ret in returns {
                    if let Left(prog) = ret {
                        if let Some(name) = prog.prog.needs_write_lock() {
                            collector.insert(name);
                        }
                    }
                }
            }
            ImperativeStmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                if let ImperativeCondition::Right(prog) = condition {
                    if let Some(name) = prog.prog.needs_write_lock() {
                        collector.insert(name);
                    }
                }
                for prog in then_branch.iter().chain(else_branch.iter()) {
                    prog.needs_write_locks(collector);
                }
            }
            ImperativeStmt::Loop { body, .. } => {
                for prog in body {
                    prog.needs_write_locks(collector);
                }
            }
            ImperativeStmt::TempDebug { .. }
            | ImperativeStmt::Break { .. }
            | ImperativeStmt::Continue { .. }
            | ImperativeStmt::TempSwap { .. } => {}
            ImperativeStmt::SysOp { sysop } => match &sysop.sysop {
                SysOp::RemoveRelation(rels) => {
                    for rel in rels {
                        collector.insert(rel.name.clone());
                    }
                }
                SysOp::RenameRelation(renames) => {
                    for (old, new) in renames {
                        collector.insert(old.name.clone());
                        collector.insert(new.name.clone());
                    }
                }
                SysOp::CreateIndex(symb, subs, _) => {
                    collector.insert(symb.name.clone());
                    collector.insert(SmartString::from(format!("{}:{}", symb.name, subs.name)));
                }
                SysOp::CreateVectorIndex(m) => {
                    collector.insert(m.base_relation.clone());
                    collector.insert(SmartString::from(format!(
                        "{}:{}",
                        m.base_relation, m.index_name
                    )));
                }
                SysOp::CreateFtsIndex(m) => {
                    collector.insert(m.base_relation.clone());
                    collector.insert(SmartString::from(format!(
                        "{}:{}",
                        m.base_relation, m.index_name
                    )));
                }
                SysOp::CreateMinHashLshIndex(m) => {
                    collector.insert(m.base_relation.clone());
                    collector.insert(SmartString::from(format!(
                        "{}:{}",
                        m.base_relation, m.index_name
                    )));
                }
                SysOp::RemoveIndex(rel, idx) => {
                    collector.insert(SmartString::from(format!("{}:{}", rel.name, idx.name)));
                }
                // mnestic fork, bitemporality step 5: these delete rows, so
                // an imperative program containing only them must still get
                // a WRITE transaction and the per-relation locks — without
                // these arms `{::evict …}` runs on a read tx (an error on
                // RocksDB, an unlocked mutation elsewhere).
                SysOp::TtHistoryGc(rel, ..) => {
                    collector.insert(rel.name.clone());
                }
                SysOp::TtEvict(rel, ..) => {
                    collector.insert(rel.name.clone());
                    // evict also writes the reserved audit relation
                    collector.insert(SmartString::from("mnestic_evict_audit"));
                }
                SysOp::Reindex(rel) | SysOp::RepairCorrupt(rel) => {
                    collector.insert(rel.name.clone());
                }
                _ => {}
            },
        }
    }
}

impl CozoScript {
    pub(crate) fn get_single_program(self) -> Result<InputProgram> {
        #[derive(Debug, Error, Diagnostic)]
        #[error("expect script to contain only a single program")]
        #[diagnostic(code(parser::expect_singleton))]
        struct ExpectSingleProgram;
        match self {
            CozoScript::Single(s) => Ok(s),
            CozoScript::Imperative(_) | CozoScript::Sys(_) => {
                bail!(ExpectSingleProgram)
            }
        }
    }
}

/// Span of the element in the source script, with starting and ending positions.
#[derive(
    Eq, PartialEq, Debug, serde_derive::Serialize, serde_derive::Deserialize, Copy, Clone, Default,
)]
pub struct SourceSpan(pub usize, pub usize);

impl Display for SourceSpan {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..{}", self.0, self.0 + self.1)
    }
}

impl SourceSpan {
    pub(crate) fn merge(self, other: Self) -> Self {
        let s1 = self.0;
        let e1 = self.0 + self.1;
        let s2 = other.0;
        let e2 = other.0 + other.1;
        let s = min(s1, s2);
        let e = max(e1, e2);
        Self(s, e - s)
    }
}

impl From<&'_ SourceSpan> for miette::SourceSpan {
    fn from(s: &'_ SourceSpan) -> Self {
        miette::SourceSpan::new(s.0.into(), s.1.into())
    }
}

impl From<SourceSpan> for miette::SourceSpan {
    fn from(s: SourceSpan) -> Self {
        miette::SourceSpan::new(s.0.into(), s.1.into())
    }
}

#[derive(thiserror::Error, Diagnostic, Debug)]
#[error("The query parser has encountered unexpected input / end of input at {span}")]
#[diagnostic(code(parser::pest))]
pub(crate) struct ParseError {
    #[label]
    pub(crate) span: SourceSpan,
    #[help]
    pub(crate) expected: Option<String>,
}

/// Past this many surviving candidate tokens the hint stops identifying a
/// defect and becomes a grammar dump, which is worse than no hint at all.
const MAX_EXPECTED_TOKENS_IN_HINT: usize = 24;

/// Convert a pest failure into our [`ParseError`], using pest's parse-attempts
/// tracking (the literal tokens the grammar would have accepted at the deepest
/// position reached) to point the span at the real defect and name the tokens
/// that would fix it. `err.location` alone is often wrong — for `?[a] a = 1` it
/// points inside the rule head while the defect (the missing `:=`) is later.
fn pest_error_to_parse_error(src: &str, err: pest::error::Error<Rule>) -> ParseError {
    let mut span = match err.location {
        InputLocation::Pos(p) => SourceSpan(p, 0),
        InputLocation::Span((start, end)) => SourceSpan(start, end - start),
    };
    let mut expected = None;
    if let Some(attempts) = err.parse_attempts() {
        span = SourceSpan(attempts.max_position, 0);
        // `ParsingToken` is not nameable outside pest (private module), so the
        // variants are distinguished through their Display forms: `Sensitive`
        // renders the literal itself, `Range` renders `a..z`, `BuiltInRule`
        // renders `BUILTIN_RULE`. Whitespace/comment openers and ranges are
        // insertable almost anywhere and carry no diagnostic value.
        let mut tokens: Vec<String> = attempts
            .expected_tokens()
            .iter()
            .map(|t| t.to_string())
            .filter(|t| {
                !t.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
                    && t != "#"
                    && t != "/*"
                    && t != "BUILTIN_RULE"
                    && !is_char_range_token(t)
            })
            .collect();
        // Mixed-context positions (e.g. after a mistyped `:limi`, where the
        // grammar could continue an expression OR start an option) produce a
        // dump of unrelated candidates. The tokens the user actually meant
        // start with the character sitting at the error position — narrow to
        // those whenever that subset is non-empty (`:limi` → the `:option`
        // keywords; `::index crate` → `create`).
        if let Some(next_char) = src
            .get(attempts.max_position..)
            .and_then(|r| r.chars().next())
        {
            let narrowed: Vec<String> = tokens
                .iter()
                .filter(|t| t.starts_with(next_char))
                .cloned()
                .collect();
            if !narrowed.is_empty() {
                tokens = narrowed;
            }
        }
        if !tokens.is_empty() && tokens.len() <= MAX_EXPECTED_TOKENS_IN_HINT {
            let quoted: Vec<String> = tokens.iter().map(|t| format!("`{t}`")).collect();
            expected = Some(if quoted.len() == 1 {
                format!("expected token: {}", quoted[0])
            } else {
                format!("expected one of: {}", quoted.join(", "))
            });
        }
    }
    ParseError { span, expected }
}

/// A pest `Range` token displays as `a..z` (single char on each side). A
/// literal `..` token would display as exactly `..` and is not caught here.
fn is_char_range_token(t: &str) -> bool {
    let chars: Vec<char> = t.chars().collect();
    chars.len() == 4 && chars[1] == '.' && chars[2] == '.'
}

pub(crate) fn parse_type(src: &str) -> Result<NullableColType> {
    let parsed = CozoScriptParser::parse(Rule::col_type_with_term, src)
        .into_diagnostic()?
        .next()
        .unwrap();
    parse_nullable_type(parsed.into_inner().next().unwrap())
}

pub(crate) fn parse_expressions(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<Expr> {
    let parsed = CozoScriptParser::parse(Rule::expression_script, src)
        .map_err(|err| pest_error_to_parse_error(src, err))?
        .next()
        .unwrap();

    build_expr(parsed.into_inner().next().unwrap(), param_pool)
}

/// This parses a text script into the AST used by Cozo.
///
/// Note! This is an unstable interface, the signature may change between releases. Depend on it at your own risk.
///
/// * `src` - the script to parse
///
/// * `param_pool` - the list of parameters to execute the script with. These are substituted into the syntax tree during parsing.
///
/// * `fixed_rules` - a mapping of fixed rule names to their implementations. These are substituted into the syntax tree during parsing.
///
/// * `cur_vld` - the current timestamp, substituted into expressions where validity is relevant.
pub fn parse_script(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<Box<dyn FixedRule>>>,
    custom_aggrs: crate::data::aggr::CustomAggrRegistries<'_>,
    cur_vld: ValidityTs,
) -> Result<CozoScript> {
    let parsed = CozoScriptParser::parse(Rule::script, src)
        .map_err(|err| pest_error_to_parse_error(src, err))?
        .next()
        .unwrap();
    Ok(match parsed.as_rule() {
        Rule::query_script => {
            let q = parse_query(
                parsed.into_inner(),
                param_pool,
                fixed_rules,
                custom_aggrs,
                cur_vld,
            )?;
            CozoScript::Single(q)
        }
        Rule::imperative_script => {
            let p = parse_imperative_block(parsed, param_pool, fixed_rules, custom_aggrs, cur_vld)?;
            CozoScript::Imperative(p)
        }

        Rule::sys_script => CozoScript::Sys(parse_sys(
            parsed.into_inner(),
            param_pool,
            fixed_rules,
            custom_aggrs,
            cur_vld,
        )?),
        _ => unreachable!(),
    })
}

trait ExtractSpan {
    fn extract_span(&self) -> SourceSpan;
}

impl ExtractSpan for Pair<'_> {
    fn extract_span(&self) -> SourceSpan {
        let span = self.as_span();
        let start = span.start();
        let end = span.end();
        SourceSpan(start, end - start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_err(src: &str) -> ParseError {
        match CozoScriptParser::parse(Rule::script, src) {
            Ok(_) => panic!("script unexpectedly parsed: {src}"),
            Err(err) => pest_error_to_parse_error(src, err),
        }
    }

    fn help(src: &str) -> String {
        parse_err(src).expected.unwrap_or_default()
    }

    // The single most common agent mistake: a rule head with no arrow. The
    // hint must name the actual missing tokens, not a grammar rule name
    // (`err.variant.positives` would have said `aggr_arg` here).
    #[test]
    fn parse_error_names_the_missing_arrow() {
        let e = parse_err("?[a] a = 1");
        let h = e.expected.clone().unwrap();
        for tok in ["`:=`", "`<-`", "`<~`"] {
            assert!(h.contains(tok), "missing {tok} in {h:?}");
        }
        assert!(!h.contains("aggr"), "rule-name leak in {h:?}");
        // ... and the caret lands on the defect (position 5), not on
        // `err.location`'s position inside the rule head (2).
        assert_eq!(e.span.0, 5);

        // same class: a fixed-rule application without its `<~`
        let h = help("?[a] BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)");
        assert!(h.contains("`<~`"), "missing `<~` in {h:?}");
    }

    // A mistyped option keyword names the real candidates: the raw expected
    // set at this position is a 40-token dump of operators AND options, and
    // the next-char narrowing is what reduces it to the `:`-keywords.
    #[test]
    fn parse_error_names_the_mistyped_option() {
        let h = help("?[a] <- [[1]]\n:limi 5");
        assert!(h.contains("`:limit`"), "missing `:limit` in {h:?}");
        assert!(
            !h.contains("`&&`"),
            "operator soup leaked past the narrowing: {h:?}"
        );
    }

    // A mistyped sysop keyword narrows to the intended token alone.
    #[test]
    fn parse_error_names_the_mistyped_sysop() {
        let h = help("::index crate rel:idx {a}");
        assert_eq!(h, "expected token: `create`");
    }

    // Structural omissions name the delimiters.
    #[test]
    fn parse_error_names_the_missing_delimiter() {
        let h = help("?[a] <~ BudgetedTraversal(e[f, t, w] s[n])");
        assert!(h.contains("`,`") && h.contains("`)`"), "{h:?}");
    }

    // When the candidate set is a mixed dump too large to identify a defect
    // (here: `:limitt` parses as `:limit t`, failing later at `5` where
    // dozens of unrelated continuations are legal and none starts with `5`),
    // the hint is suppressed entirely — an unactionable hint is the failure
    // mode this feature exists to kill.
    #[test]
    fn parse_error_suppresses_grammar_dumps() {
        assert_eq!(parse_err("?[a] <- [[1]]\n:limitt 5").expected, None);
    }
}
