/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! RdfReader — boundary reader for the Turtle family of RDF formats
//! (mnestic fork; spec: `docs/specs/rdf-boundary-io.md`).
//!
//! Reads Turtle, N-Triples, N-Quads, or TriG into a fixed 6-column relational
//! shape — RDF converts to ordinary relations at the edge, and nothing inside
//! the engine knows the rows came from RDF:
//!
//! ```cozo
//! triples[s, p, o, g, lang, dt] <~ RdfReader(url: 'file://./data.ttl')
//! ?[s, o] := triples[s, 'http://xmlns.com/foaf/0.1/knows', o, _, _, _]
//! ```
//!
//! Output columns, in order: `subject, predicate, object, graph, language_tag,
//! datatype` — all `Str` or `Null`. `subject`, `predicate`, `object` are always
//! populated; the rest are nullable. Triple formats emit `Null` graphs; quad
//! formats (N-Quads, TriG) fill them. IRIs are plain strings (no wrapping);
//! blank nodes keep their `_:label` lexical form by default (labels are
//! file-scoped — two files' `_:b0` are different nodes; see `skolemize`);
//! literals carry their lexical form in `object` with `language_tag`/`datatype`
//! carrying what the syntax carried. Plain literals have both `Null`, and an
//! explicit `xsd:string` datatype is normalized to `Null` (RDF 1.1 semantics:
//! a plain literal and an `xsd:string` literal are the same term — spec §12
//! Q4). **No coercion of typed literals** — `"42"^^xsd:integer` stays the
//! string `"42"` with its datatype column set.
//!
//! Options:
//! - `url` (required): `file://` prefix reads a local path; anything else
//!   requires the `requests` feature (the CsvReader split, verbatim).
//! - `format`: one of `'turtle'`, `'ntriples'`, `'nquads'`, `'trig'`.
//!   Defaults from the URL extension (`.ttl`/`.nt`/`.nq`/`.trig`); it is an
//!   error if neither determines the format.
//! - `base`: base IRI for relative-IRI resolution (Turtle/TriG only).
//! - `prefixes`: extra prefix declarations, as a JSON object mapping prefix
//!   name to IRI (e.g. `prefixes: parse_json('{"foaf": "http://…/"}')`) or a
//!   list of `[prefix, iri]` pairs (Turtle/TriG only).
//! - `prepend_index` (default `false`): prepend a 0-based row counter,
//!   CsvReader-style; arity becomes 7.
//! - `skolemize`: a namespace IRI. Each blank node `_:label` is rewritten to
//!   the deterministic IRI `<namespace><uuid-v5>`, where the v5 input is
//!   salted with the source (`url`) — so re-loads of the same source agree
//!   and different sources disagree (idempotent re-loads; cross-file joins
//!   become sound).
//!
//! Errors abort on the first syntax error, with the parser's message and byte
//! position — oxttl's error-recovery and `lenient()` modes are deliberately
//! not exposed. The parse itself streams; one parser instance per invocation
//! (blank-node label scope is the document). The poison flag is consulted
//! every 4,096 parsed statements — an intentional improvement on the shipped
//! readers, which never check it.

use std::collections::BTreeMap;
use std::io::Read;

use miette::{bail, IntoDiagnostic, Result, WrapErr};
use oxrdf::{GraphName, NamedNode, NamedOrBlankNode, Term};
use oxttl::{NQuadsParser, NTriplesParser, TriGParser, TurtleParseError, TurtleParser};
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::program::WrongFixedRuleOptionError;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, JsonData};
#[cfg(feature = "requests")]
use crate::fixed_rule::utilities::jlines::get_file_content_from_url;
use crate::fixed_rule::{CannotDetermineArity, FixedRule, FixedRulePayload};
use crate::parse::SourceSpan;
use crate::runtime::db::Poison;
use crate::runtime::temp_store::RegularTempStore;

pub(crate) struct RdfReader;

/// Poison-check cadence, in parsed statements (spec §3; pinned by tests).
const POISON_CHECK_EVERY: u64 = 4096;

pub(crate) const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RdfFormat {
    Turtle,
    NTriples,
    NQuads,
    TriG,
}

impl RdfFormat {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "turtle" => RdfFormat::Turtle,
            "ntriples" => RdfFormat::NTriples,
            "nquads" => RdfFormat::NQuads,
            "trig" => RdfFormat::TriG,
            _ => return None,
        })
    }

    fn from_url(url: &str) -> Option<Self> {
        // Extension sniffing ignores any query/fragment suffix.
        let path = url.split(['?', '#']).next().unwrap_or(url);
        Some(match () {
            _ if path.ends_with(".ttl") => RdfFormat::Turtle,
            _ if path.ends_with(".nt") => RdfFormat::NTriples,
            _ if path.ends_with(".nq") => RdfFormat::NQuads,
            _ if path.ends_with(".trig") => RdfFormat::TriG,
            _ => return None,
        })
    }

    /// The line-oriented formats have no directives: `base`/`prefixes` are
    /// meaningless there and passing them is a loud error, not a no-op.
    fn is_line_format(self) -> bool {
        matches!(self, RdfFormat::NTriples | RdfFormat::NQuads)
    }
}

/// Deterministic, source-salted blank-node skolemization (spec §5, §12 Q2).
struct Skolem {
    namespace: String,
    ns_uuid: uuid::Uuid,
    salt: String,
    cache: BTreeMap<String, SmartString<LazyCompact>>,
}

impl Skolem {
    fn new(namespace: String, salt: String) -> Self {
        // Derive the v5 namespace UUID from the namespace IRI itself, under
        // the standard URL namespace.
        let ns_uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, namespace.as_bytes());
        Skolem {
            namespace,
            ns_uuid,
            salt,
            cache: BTreeMap::new(),
        }
    }

    fn iri_for(&mut self, label: &str) -> DataValue {
        if let Some(hit) = self.cache.get(label) {
            return DataValue::Str(hit.clone());
        }
        // NUL separates salt from label (neither may contain it), so
        // (salt="a", label="bc") and (salt="ab", label="c") never collide.
        let name = format!("{}\u{0}{}", self.salt, label);
        let id = uuid::Uuid::new_v5(&self.ns_uuid, name.as_bytes());
        let iri = SmartString::from(format!("{}{}", self.namespace, id));
        self.cache.insert(label.to_string(), iri.clone());
        DataValue::Str(iri)
    }
}

struct Emitter<'a> {
    out: &'a mut RegularTempStore,
    poison: Poison,
    prepend_index: bool,
    counter: i64,
    parsed: u64,
    skolem: Option<Skolem>,
}

impl Emitter<'_> {
    fn bnode_value(&mut self, label: &str) -> DataValue {
        match &mut self.skolem {
            None => DataValue::Str(SmartString::from(format!("_:{label}"))),
            Some(sk) => sk.iri_for(label),
        }
    }

    /// `graph`: `None` for the triple formats (column stays `Null`),
    /// `Some(graph_name)` for the quad formats.
    fn emit(
        &mut self,
        subject: NamedOrBlankNode,
        predicate: NamedNode,
        object: Term,
        graph: Option<GraphName>,
    ) -> Result<()> {
        self.parsed += 1;
        if self.parsed % POISON_CHECK_EVERY == 0 {
            self.poison.check()?;
        }
        let s = match subject {
            NamedOrBlankNode::NamedNode(n) => DataValue::Str(n.into_string().into()),
            NamedOrBlankNode::BlankNode(b) => self.bnode_value(b.as_str()),
        };
        let p = DataValue::Str(predicate.into_string().into());
        let (o, lang, dt) = match object {
            Term::NamedNode(n) => (
                DataValue::Str(n.into_string().into()),
                DataValue::Null,
                DataValue::Null,
            ),
            Term::BlankNode(b) => (
                self.bnode_value(b.as_str()),
                DataValue::Null,
                DataValue::Null,
            ),
            Term::Literal(lit) => {
                let (value, datatype, language) = lit.destruct();
                let lang = match language {
                    Some(l) => DataValue::Str(l.into()),
                    None => DataValue::Null,
                };
                // Q4: `xsd:string` and a plain literal are the same RDF 1.1
                // term — normalize the datatype to Null.
                let dt = match datatype {
                    Some(d) if d.as_str() == XSD_STRING => DataValue::Null,
                    Some(d) => DataValue::Str(d.into_string().into()),
                    None => DataValue::Null,
                };
                (DataValue::Str(value.into()), lang, dt)
            }
        };
        let g = match graph {
            None | Some(GraphName::DefaultGraph) => DataValue::Null,
            Some(GraphName::NamedNode(n)) => DataValue::Str(n.into_string().into()),
            Some(GraphName::BlankNode(b)) => self.bnode_value(b.as_str()),
        };
        let mut row = Vec::with_capacity(if self.prepend_index { 7 } else { 6 });
        if self.prepend_index {
            self.counter += 1;
            row.push(DataValue::from(self.counter));
        }
        row.extend([s, p, o, g, lang, dt]);
        self.out.put(row);
        Ok(())
    }
}

/// First-error abort, carrying the parser's own message (which names line and
/// column) plus the byte offset and the source URL.
fn parse_failure(url: &str, e: TurtleParseError) -> miette::Report {
    match e {
        TurtleParseError::Syntax(e) => {
            let offset = e.location().start.offset;
            miette::miette!("RDF parse error in {url} (byte offset {offset}): {e}")
        }
        TurtleParseError::Io(e) => miette::miette!("I/O error while reading RDF from {url}: {e}"),
    }
}

fn bad_option(name: &str, span: SourceSpan, help: impl Into<String>) -> miette::Report {
    WrongFixedRuleOptionError {
        name: name.to_string(),
        span,
        rule_name: "RdfReader".to_string(),
        help: help.into(),
    }
    .into()
}

#[allow(clippy::too_many_arguments)]
fn parse_from<R: Read>(
    format: RdfFormat,
    base: Option<&str>,
    prefixes: &[(String, String)],
    reader: R,
    emitter: &mut Emitter<'_>,
    url: &str,
    span: SourceSpan,
) -> Result<()> {
    match format {
        RdfFormat::Turtle => {
            let mut parser = TurtleParser::new();
            if let Some(b) = base {
                parser = parser
                    .with_base_iri(b)
                    .map_err(|e| bad_option("base", span, format!("invalid base IRI: {e}")))?;
            }
            for (name, iri) in prefixes {
                parser = parser.with_prefix(name.clone(), iri.clone()).map_err(|e| {
                    bad_option(
                        "prefixes",
                        span,
                        format!("invalid prefix IRI for '{name}': {e}"),
                    )
                })?;
            }
            for triple in parser.for_reader(reader) {
                let t = triple.map_err(|e| parse_failure(url, e))?;
                emitter.emit(t.subject, t.predicate, t.object, None)?;
            }
        }
        RdfFormat::NTriples => {
            for triple in NTriplesParser::new().for_reader(reader) {
                let t = triple.map_err(|e| parse_failure(url, e))?;
                emitter.emit(t.subject, t.predicate, t.object, None)?;
            }
        }
        RdfFormat::NQuads => {
            for quad in NQuadsParser::new().for_reader(reader) {
                let q = quad.map_err(|e| parse_failure(url, e))?;
                emitter.emit(q.subject, q.predicate, q.object, Some(q.graph_name))?;
            }
        }
        RdfFormat::TriG => {
            let mut parser = TriGParser::new();
            if let Some(b) = base {
                parser = parser
                    .with_base_iri(b)
                    .map_err(|e| bad_option("base", span, format!("invalid base IRI: {e}")))?;
            }
            for (name, iri) in prefixes {
                parser = parser.with_prefix(name.clone(), iri.clone()).map_err(|e| {
                    bad_option(
                        "prefixes",
                        span,
                        format!("invalid prefix IRI for '{name}': {e}"),
                    )
                })?;
            }
            for quad in parser.for_reader(reader) {
                let q = quad.map_err(|e| parse_failure(url, e))?;
                emitter.emit(q.subject, q.predicate, q.object, Some(q.graph_name))?;
            }
        }
    }
    Ok(())
}

impl FixedRule for RdfReader {
    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &mut RegularTempStore,
        poison: Poison,
    ) -> Result<()> {
        let span = payload.span();
        let url = payload.string_option("url", None)?;

        let format_opt = payload.string_option("format", Some(""))?;
        let format = if format_opt.is_empty() {
            match RdfFormat::from_url(&url) {
                Some(f) => f,
                None => {
                    return Err(bad_option(
                        "format",
                        span,
                        "cannot determine the RDF format: pass format: 'turtle', 'ntriples', \
                         'nquads' or 'trig', or use a url ending in .ttl/.nt/.nq/.trig",
                    ))
                }
            }
        } else {
            match RdfFormat::from_name(&format_opt) {
                Some(f) => f,
                None => {
                    return Err(bad_option(
                        "format",
                        span,
                        "expected one of 'turtle', 'ntriples', 'nquads', 'trig'",
                    ))
                }
            }
        };

        let base = payload.string_option("base", Some(""))?;
        let base = if base.is_empty() {
            None
        } else {
            Some(base.to_string())
        };

        let prefixes_expr = payload.expr_option(
            "prefixes",
            Some(Expr::Const {
                val: DataValue::Null,
                span,
            }),
        )?;
        let prefixes: Vec<(String, String)> = match prefixes_expr.eval_to_const()? {
            DataValue::Null => vec![],
            DataValue::Json(JsonData(serde_json::Value::Object(m))) => {
                let mut entries = Vec::with_capacity(m.len());
                for (name, iri) in m {
                    match iri {
                        serde_json::Value::String(iri) => entries.push((name, iri)),
                        _ => {
                            return Err(bad_option(
                                "prefixes",
                                span,
                                format!("prefix '{name}' must map to an IRI string"),
                            ))
                        }
                    }
                }
                entries
            }
            DataValue::List(pairs) => {
                let mut entries = Vec::with_capacity(pairs.len());
                for pair in pairs {
                    match pair.get_slice() {
                        Some([DataValue::Str(name), DataValue::Str(iri)]) => {
                            entries.push((name.to_string(), iri.to_string()))
                        }
                        _ => {
                            return Err(bad_option(
                                "prefixes",
                                span,
                                "expected a list of [prefix, iri] string pairs",
                            ))
                        }
                    }
                }
                entries
            }
            _ => {
                return Err(bad_option(
                    "prefixes",
                    span,
                    "expected a JSON object mapping prefix names to IRIs, or a list of \
                     [prefix, iri] pairs",
                ))
            }
        };

        if format.is_line_format() {
            if base.is_some() {
                return Err(bad_option(
                    "base",
                    span,
                    "'base' is not applicable to the line-oriented formats \
                     (ntriples/nquads): they admit no relative IRIs",
                ));
            }
            if !prefixes.is_empty() {
                return Err(bad_option(
                    "prefixes",
                    span,
                    "'prefixes' is not applicable to the line-oriented formats \
                     (ntriples/nquads): they admit no prefixed names",
                ));
            }
        }

        let prepend_index = payload.bool_option("prepend_index", Some(false))?;

        let skolemize = payload.string_option("skolemize", Some(""))?;
        let skolem = if skolemize.is_empty() {
            None
        } else {
            oxiri::Iri::parse(skolemize.as_str()).map_err(|e| {
                bad_option(
                    "skolemize",
                    span,
                    format!("must be a valid absolute IRI namespace: {e}"),
                )
            })?;
            // Salt = the resolved url string (spec §5): same source ⇒ same
            // IRIs, different source ⇒ different.
            Some(Skolem::new(skolemize.to_string(), url.to_string()))
        };

        let mut emitter = Emitter {
            out,
            poison,
            prepend_index,
            counter: -1,
            parsed: 0,
            skolem,
        };

        match url.strip_prefix("file://") {
            Some(file_path) => {
                let file = std::fs::File::open(file_path)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("when opening {file_path}"))?;
                parse_from(
                    format,
                    base.as_deref(),
                    &prefixes,
                    std::io::BufReader::new(file),
                    &mut emitter,
                    &url,
                    span,
                )?;
            }
            None => {
                #[cfg(feature = "requests")]
                {
                    let content = get_file_content_from_url(&url)?;
                    parse_from(
                        format,
                        base.as_deref(),
                        &prefixes,
                        content.as_bytes(),
                        &mut emitter,
                        &url,
                        span,
                    )?;
                }
                #[cfg(not(feature = "requests"))]
                bail!("the feature `requests` is not enabled for the build")
            }
        }
        Ok(())
    }

    fn arity(
        &self,
        options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        span: SourceSpan,
    ) -> Result<usize> {
        let with_row_num = match options.get("prepend_index") {
            None => 0,
            Some(Expr::Const {
                val: DataValue::Bool(true),
                ..
            }) => 1,
            Some(Expr::Const {
                val: DataValue::Bool(false),
                ..
            }) => 0,
            _ => bail!(CannotDetermineArity(
                "RdfReader".to_string(),
                "invalid option 'prepend_index' given, expect a boolean".to_string(),
                span
            )),
        };
        Ok(6 + with_row_num)
    }
}
