/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use crate::fts::tokenizer::TextAnalyzer;
use miette::{bail, Diagnostic, Result};
use ordered_float::OrderedFloat;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FtsLiteral {
    pub(crate) value: SmartString<LazyCompact>,
    pub(crate) is_prefix: bool,
    pub(crate) booster: OrderedFloat<f64>,
    /// True iff the literal was written quoted (`"..."`, `'...'`, raw string)
    /// in the query. A quoted literal that tokenizes to ≥ 2 tokens is an exact
    /// phrase (`FtsExpr::Phrase`); unquoted literals keep AND-of-terms
    /// semantics. Recorded at parse time (`parse/fts.rs::build_phrase`).
    pub(crate) is_phrase: bool,
}

impl FtsLiteral {
    pub(crate) fn tokenize(self, tokenizer: &TextAnalyzer, coll: &mut Vec<Self>) {
        if self.is_prefix {
            coll.push(self);
            return;
        }

        let mut tokens = tokenizer.token_stream(&self.value);
        while let Some(t) = tokens.next() {
            coll.push(FtsLiteral {
                value: SmartString::from(&t.text),
                is_prefix: false,
                booster: self.booster,
                is_phrase: false,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FtsNear {
    pub(crate) literals: Vec<FtsLiteral>,
    pub(crate) distance: u32,
}

/// One token of an exact phrase, carrying the position the query-side analyzer
/// assigned it. Positions are source-ordinal and survive every filter (the
/// tokenizer numbers tokens before filters run, and filters like `Stopwords`
/// skip without renumbering), so a removed stopword leaves a hole — which is
/// what makes phrase matching correct under stopwords: the hole is simply an
/// unconstrained one-token slot, symmetrically on the query and document side.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FtsPhraseToken {
    pub(crate) value: SmartString<LazyCompact>,
    pub(crate) position: u32,
}

/// An exact-phrase query: a quoted literal whose tokenization produced ≥ 2
/// tokens. A document matches at anchor `p` iff for every token with query
/// position `qᵢ` it contains that token at exactly `p + (qᵢ − q₀)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FtsPhrase {
    pub(crate) tokens: Vec<FtsPhraseToken>,
    pub(crate) booster: OrderedFloat<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum FtsExpr {
    Literal(FtsLiteral),
    Phrase(FtsPhrase),
    Near(FtsNear),
    And(Vec<FtsExpr>),
    Or(Vec<FtsExpr>),
    Not(Box<FtsExpr>, Box<FtsExpr>),
}

fn tokenize_with_positions(value: &str, tokenizer: &TextAnalyzer) -> Vec<FtsPhraseToken> {
    let mut stream = tokenizer.token_stream(value);
    let mut out = vec![];
    while let Some(t) = stream.next() {
        out.push(FtsPhraseToken {
            value: SmartString::from(&t.text),
            position: t.position as u32,
        });
    }
    out
}

#[derive(Debug, Diagnostic, Error)]
#[error(
    "phrase-prefix queries (a quoted multi-word phrase with a `*` marker) are not yet supported"
)]
#[diagnostic(
    code(parser::fts::phrase_prefix_unsupported),
    help(
        "split it: `\"{0}\" OR {1}*` searches the exact phrase or the prefix term. \
          (Before 0.13.2 this query silently matched nothing: the whole quoted string \
          was prefix-matched against single-token index entries. True phrase-prefix \
          search is tracked at https://github.com/shuruheel/mnestic/issues/19.)"
    )
)]
struct FtsPhrasePrefixUnsupported(String, String);

#[derive(Debug, Diagnostic, Error)]
#[error("a quoted multi-word phrase inside NEAR(...) is not yet supported")]
#[diagnostic(
    code(parser::fts::phrase_in_near_unsupported),
    help(
        "NEAR treats every operand as an unordered bag of single terms, so quoting \
          \"{0}\" inside it would silently drop the adjacency requirement. Use the \
          phrase and the NEAR group as separate AND-ed query parts, or list the \
          words unquoted to keep bag-of-terms proximity. (Phrase-in-NEAR is tracked \
          at https://github.com/shuruheel/mnestic/issues/20.)"
    )
)]
struct FtsPhraseInNearUnsupported(String);

impl FtsExpr {
    // pub(crate) fn needs_idf(&self) -> bool {
    //     match self {
    //         FtsExpr::Literal(_) => false,
    //         FtsExpr::Near(_) => false,
    //         FtsExpr::And(exprs) => exprs.iter().any(|e| e.needs_idf()),
    //         FtsExpr::Or(_) => true,
    //         FtsExpr::Not(lhs, _) => lhs.needs_idf(),
    //     }
    // }

    pub(crate) fn tokenize(self, tokenizer: &TextAnalyzer) -> Result<Self> {
        Ok(self.do_tokenize(tokenizer)?.flatten())
    }

    /// Whether any part of the query is an exact phrase — used by the eval
    /// entry to refuse phrase queries on position-degenerate indexes (NGram
    /// assigns every token position 0, so adjacency is unfalsifiable and a
    /// phrase would match every document containing the tokens).
    pub(crate) fn contains_phrase(&self) -> bool {
        match self {
            FtsExpr::Phrase(_) => true,
            FtsExpr::Literal(_) | FtsExpr::Near(_) => false,
            FtsExpr::And(v) | FtsExpr::Or(v) => v.iter().any(|e| e.contains_phrase()),
            FtsExpr::Not(lhs, rhs) => lhs.contains_phrase() || rhs.contains_phrase(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        match self {
            FtsExpr::Literal(l) => l.booster == 0. || l.value.is_empty(),
            FtsExpr::Phrase(p) => p.booster == 0. || p.tokens.is_empty(),
            FtsExpr::Near(FtsNear { literals, .. }) => literals.is_empty(),
            FtsExpr::And(v) => v.is_empty(),
            FtsExpr::Or(v) => v.is_empty(),
            FtsExpr::Not(lhs, _) => lhs.is_empty(),
        }
    }

    pub(crate) fn flatten(self) -> Self {
        match self {
            FtsExpr::And(exprs) => {
                let mut flattened = vec![];
                for e in exprs {
                    match e.flatten() {
                        FtsExpr::And(es) => flattened.extend(es),
                        e => {
                            if !e.is_empty() {
                                flattened.push(e)
                            }
                        }
                    }
                }
                if flattened.len() == 1 {
                    flattened.into_iter().next().unwrap()
                } else {
                    FtsExpr::And(flattened)
                }
            }
            FtsExpr::Or(exprs) => {
                let mut flattened = vec![];
                for e in exprs {
                    match e.flatten() {
                        FtsExpr::Or(es) => flattened.extend(es),
                        e => {
                            if !e.is_empty() {
                                flattened.push(e)
                            }
                        }
                    }
                }
                if flattened.len() == 1 {
                    flattened.into_iter().next().unwrap()
                } else {
                    FtsExpr::Or(flattened)
                }
            }
            FtsExpr::Not(lhs, rhs) => {
                let lhs = lhs.flatten();
                let rhs = rhs.flatten();
                if rhs.is_empty() {
                    lhs
                } else {
                    FtsExpr::Not(Box::new(lhs), Box::new(rhs))
                }
            }
            FtsExpr::Literal(l) => FtsExpr::Literal(l),
            FtsExpr::Phrase(p) => FtsExpr::Phrase(p),
            FtsExpr::Near(n) => FtsExpr::Near(n),
        }
    }

    fn do_tokenize(self, tokenizer: &TextAnalyzer) -> Result<Self> {
        Ok(match self {
            FtsExpr::Literal(l) => {
                if l.is_phrase && !l.value.is_empty() {
                    // A quoted literal. Tokenize position-aware to decide what
                    // it is: ≥ 2 tokens ⇒ an exact phrase; 1 token ⇒ plain
                    // Literal (so `"fox"` ≡ `fox`, including `"fox"*` prefix);
                    // 0 tokens (e.g. all stopwords) ⇒ empty, culled by flatten.
                    let toks = tokenize_with_positions(&l.value, tokenizer);
                    if toks.len() >= 2 {
                        if l.is_prefix {
                            let last = toks.last().unwrap().value.to_string();
                            bail!(FtsPhrasePrefixUnsupported(l.value.to_string(), last));
                        }
                        return Ok(FtsExpr::Phrase(FtsPhrase {
                            tokens: toks,
                            booster: l.booster,
                        }));
                    }
                    // Fall through to the single-term path below, preserving
                    // today's semantics exactly (including prefix matching on
                    // the raw value when `is_prefix` short-circuits).
                }
                let mut tokens = vec![];
                l.tokenize(tokenizer, &mut tokens);
                if tokens.len() == 1 {
                    FtsExpr::Literal(tokens.into_iter().next().unwrap())
                } else {
                    FtsExpr::And(tokens.into_iter().map(FtsExpr::Literal).collect())
                }
            }
            // Phrase only exists post-tokenization (built in the Literal arm
            // above); if one ever arrives here it is already tokenized.
            FtsExpr::Phrase(p) => FtsExpr::Phrase(p),
            FtsExpr::Near(FtsNear { literals, distance }) => {
                let mut tokens = vec![];
                for l in literals {
                    if l.is_phrase
                        && !l.is_prefix
                        && tokenize_with_positions(&l.value, tokenizer).len() >= 2
                    {
                        // A quoted multi-word phrase inside NEAR would silently
                        // degrade to its bag of tokens (adjacency dropped) —
                        // the bug class 0.13.2 removes. Refuse loudly instead.
                        bail!(FtsPhraseInNearUnsupported(l.value.to_string()));
                    }
                    l.tokenize(tokenizer, &mut tokens);
                }
                FtsExpr::Near(FtsNear {
                    literals: tokens,
                    distance,
                })
            }
            FtsExpr::And(exprs) => FtsExpr::And(
                exprs
                    .into_iter()
                    .map(|e| e.do_tokenize(tokenizer))
                    .collect::<Result<_>>()?,
            ),
            FtsExpr::Or(exprs) => FtsExpr::Or(
                exprs
                    .into_iter()
                    .map(|e| e.do_tokenize(tokenizer))
                    .collect::<Result<_>>()?,
            ),
            FtsExpr::Not(lhs, rhs) => FtsExpr::Not(
                Box::new(lhs.do_tokenize(tokenizer)?),
                Box::new(rhs.do_tokenize(tokenizer)?),
            ),
        })
    }
}
