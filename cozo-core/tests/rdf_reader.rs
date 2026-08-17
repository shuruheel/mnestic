/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! RDF boundary I/O test matrix (`docs/specs/rdf-boundary-io.md` §11):
//! `RdfReader` format coverage, literal fidelity, blank-node defaults and
//! skolemization, base/prefix resolution, first-error abort, option/arity
//! validation, and round-trips through `export_relation_as_rdf`.
//!
//! Uses the sqlite backend with a tempdir per the repo test-backend rule (the
//! `mem` backend takes a different stored-join path). The whole file is gated
//! on `rdf-io`; the registration-gating assertion that must hold in BOTH
//! feature matrices lives in `tests/data_import_security.rs`.

#![cfg(feature = "rdf-io")]

use std::collections::BTreeMap;

use cozo::{DbInstance, NamedRows, ScriptMutability};

fn make_db(dir: &tempfile::TempDir, name: &str) -> DbInstance {
    let path = dir.path().join(name);
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {script}\n{e:?}"))
}

fn run_err(db: &DbInstance, script: &str) -> String {
    match db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable) {
        Ok(_) => panic!("script unexpectedly succeeded: {script}"),
        Err(e) => format!("{e:?}"),
    }
}

fn write_fixture(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
    let p = dir.path().join(name);
    std::fs::write(&p, content).unwrap();
    format!("file://{}", p.to_str().unwrap())
}

/// Read a URL through RdfReader and return the JSON rows, sorted for
/// order-independent comparison. `extra` is spliced verbatim after the url
/// option (e.g. `", skolemize: 'http://…/'"`).
fn read_rows(db: &DbInstance, url: &str, extra: &str) -> Vec<serde_json::Value> {
    let script = format!("?[s, p, o, g, lang, dt] <~ RdfReader(url: '{url}'{extra})");
    let mut rows = run(db, &script).into_json()["rows"]
        .as_array()
        .unwrap()
        .clone();
    rows.sort_by_key(|r| r.to_string());
    rows
}

// ---------------------------------------------------------------- §11 test 1

#[test]
fn format_coverage_all_four_formats() {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(&dir, "formats.db");

    // Turtle: prefixes, base-relative IRIs, a typed literal.
    let ttl = write_fixture(
        &dir,
        "data.ttl",
        r#"@base <http://example.com/> .
@prefix foaf: <http://xmlns.com/foaf/0.1/> .
<alice> foaf:name "Alice" .
<alice> foaf:age "33"^^<http://www.w3.org/2001/XMLSchema#integer> .
"#,
    );
    let rows = read_rows(&db, &ttl, "");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        serde_json::json!([
            "http://example.com/alice",
            "http://xmlns.com/foaf/0.1/age",
            "33",
            null,
            null,
            "http://www.w3.org/2001/XMLSchema#integer"
        ])
    );
    assert_eq!(
        rows[1],
        serde_json::json!([
            "http://example.com/alice",
            "http://xmlns.com/foaf/0.1/name",
            "Alice",
            null,
            null,
            null
        ])
    );

    // N-Triples: graph column stays Null.
    let nt = write_fixture(
        &dir,
        "data.nt",
        "<http://example.com/a> <http://example.com/p> \"v\" .\n",
    );
    assert_eq!(
        read_rows(&db, &nt, ""),
        vec![serde_json::json!([
            "http://example.com/a",
            "http://example.com/p",
            "v",
            null,
            null,
            null
        ])]
    );

    // N-Quads: named graph filled, default-graph statement stays Null.
    let nq = write_fixture(
        &dir,
        "data.nq",
        "<http://example.com/a> <http://example.com/p> \"v\" <http://example.com/g1> .\n\
         <http://example.com/b> <http://example.com/p> \"w\" .\n",
    );
    let rows = read_rows(&db, &nq, "");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][3], serde_json::json!("http://example.com/g1"));
    assert_eq!(rows[1][3], serde_json::json!(null));

    // TriG: graph blocks fill the column; top-level statements do not.
    let trig = write_fixture(
        &dir,
        "data.trig",
        r#"@prefix ex: <http://example.com/> .
ex:g1 { ex:a ex:p "v" . }
ex:b ex:p "w" .
"#,
    );
    let rows = read_rows(&db, &trig, "");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][3], serde_json::json!("http://example.com/g1"));
    assert_eq!(rows[1][3], serde_json::json!(null));
}

// ---------------------------------------------------------------- §11 test 3

#[test]
fn literal_fidelity_and_xsd_string_normalization() {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(&dir, "literals.db");
    let nt = write_fixture(
        &dir,
        "lit.nt",
        "<http://e.com/x> <http://e.com/p1> \"plain\" .\n\
         <http://e.com/x> <http://e.com/p2> \"tagged\"@en .\n\
         <http://e.com/x> <http://e.com/p3> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n\
         <http://e.com/x> <http://e.com/p4> \"explicit\"^^<http://www.w3.org/2001/XMLSchema#string> .\n",
    );
    let rows = read_rows(&db, &nt, "");
    let by_pred = |p: &str| {
        rows.iter()
            .find(|r| r[1] == serde_json::json!(format!("http://e.com/{p}")))
            .unwrap()
            .clone()
    };
    // Plain literal: both Null.
    let r = by_pred("p1");
    assert_eq!(
        (&r[2], &r[4], &r[5]),
        (
            &serde_json::json!("plain"),
            &serde_json::json!(null),
            &serde_json::json!(null)
        )
    );
    // Language-tagged: tag set, datatype Null.
    let r = by_pred("p2");
    assert_eq!(
        (&r[2], &r[4], &r[5]),
        (
            &serde_json::json!("tagged"),
            &serde_json::json!("en"),
            &serde_json::json!(null)
        )
    );
    // Typed literal: lexical form survives uncoerced, datatype carried.
    let r = by_pred("p3");
    assert_eq!(
        (&r[2], &r[4], &r[5]),
        (
            &serde_json::json!("42"),
            &serde_json::json!(null),
            &serde_json::json!("http://www.w3.org/2001/XMLSchema#integer")
        )
    );
    // Q4: explicit xsd:string normalizes to Null — same term as a plain literal.
    let r = by_pred("p4");
    assert_eq!(
        (&r[2], &r[4], &r[5]),
        (
            &serde_json::json!("explicit"),
            &serde_json::json!(null),
            &serde_json::json!(null)
        )
    );
}

// ---------------------------------------------------------------- §11 test 4

#[test]
fn blank_nodes_default_and_skolemize_stability() {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(&dir, "bnodes.db");
    let content = "_:b0 <http://e.com/p> \"v\" .\n\
                   _:b0 <http://e.com/q> _:b1 .\n";
    let url_a = write_fixture(&dir, "bn_a.nt", content);
    let url_b = write_fixture(&dir, "bn_b.nt", content);

    // Default: `_:label` lexical forms, unchanged.
    let rows = read_rows(&db, &url_a, "");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], serde_json::json!("_:b0"));
    assert_eq!(rows[1][2], serde_json::json!("_:b1"));

    // Skolemized: IRIs under the namespace, stable across re-loads of the
    // same source, distinct across sources.
    let ns = "http://example.com/.well-known/genid/";
    let opt = format!(", skolemize: '{ns}'");
    let first = read_rows(&db, &url_a, &opt);
    let again = read_rows(&db, &url_a, &opt);
    assert_eq!(first, again, "same source must skolemize identically");
    for row in &first {
        let s = row[0].as_str().unwrap();
        assert!(s.starts_with(ns), "skolem IRI {s} not under namespace");
        assert!(!s.starts_with("_:"));
    }
    let other = read_rows(&db, &url_b, &opt);
    assert_ne!(
        first, other,
        "different sources must mint different skolem IRIs"
    );
    // The two loads still agree on everything except the blank-node cells.
    assert_eq!(first.len(), other.len());
    for (a, b) in first.iter().zip(&other) {
        assert_eq!(a[1], b[1]);
        assert_eq!(a[3], b[3]);
    }

    // A bad namespace is a loud option error.
    let err = run_err(
        &db,
        &format!("?[s, p, o, g, lang, dt] <~ RdfReader(url: '{url_a}', skolemize: 'not an iri')"),
    );
    assert!(err.contains("skolemize"), "unexpected error: {err}");
}

// ---------------------------------------------------------------- §11 test 5

#[test]
fn base_and_prefix_options_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(&dir, "baseprefix.db");
    // Neither @base nor @prefix in the document: both arrive as options.
    let ttl = write_fixture(&dir, "rel.ttl", "<a> foaf:name \"A\" .\n");
    let rows = read_rows(
        &db,
        &ttl,
        ", base: 'http://base.org/dir/', \
         prefixes: parse_json('{\"foaf\": \"http://xmlns.com/foaf/0.1/\"}')",
    );
    assert_eq!(
        rows,
        vec![serde_json::json!([
            "http://base.org/dir/a",
            "http://xmlns.com/foaf/0.1/name",
            "A",
            null,
            null,
            null
        ])]
    );

    // Without a base, the relative IRI is a parse error (first-error abort).
    let err = run_err(
        &db,
        &format!("?[s, p, o, g, lang, dt] <~ RdfReader(url: '{ttl}')"),
    );
    assert!(err.contains("RDF parse error"), "unexpected error: {err}");

    // The line-oriented formats loudly reject the directive options.
    let nt = write_fixture(
        &dir,
        "flat.nt",
        "<http://e.com/a> <http://e.com/p> \"v\" .\n",
    );
    let err = run_err(
        &db,
        &format!("?[s, p, o, g, lang, dt] <~ RdfReader(url: '{nt}', base: 'http://e.com/')"),
    );
    assert!(err.contains("not applicable"), "unexpected error: {err}");
    let err = run_err(
        &db,
        &format!(
            "?[s, p, o, g, lang, dt] <~ RdfReader(url: '{nt}', \
             prefixes: parse_json('{{\"ex\": \"http://e.com/\"}}'))"
        ),
    );
    assert!(err.contains("not applicable"), "unexpected error: {err}");
}

// ---------------------------------------------------------------- §11 test 7

#[test]
fn first_error_aborts_with_position_and_no_partial_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(&dir, "abort.db");
    let bad = write_fixture(
        &dir,
        "bad.nt",
        "<http://e.com/a> <http://e.com/p> \"ok\" .\n\
         this line is not N-Triples\n",
    );
    run(
        &db,
        ":create sink {s: String, p: String, o: String, g: String?, lang: String?, dt: String?}",
    );
    let err = run_err(
        &db,
        &format!(
            "?[s, p, o, g, lang, dt] <~ RdfReader(url: '{bad}') \
             :put sink {{s, p, o, g, lang, dt}}"
        ),
    );
    assert!(
        err.contains("RDF parse error") && err.contains("byte offset"),
        "error must carry the parser message and byte position, got: {err}"
    );
    // The failed script left nothing behind: no partial rows observable.
    let count = run(&db, "?[count(s)] := *sink[s, p, o, g, lang, dt]");
    assert_eq!(count.into_json()["rows"], serde_json::json!([[0]]));
}

// ---------------------------------------------------------------- §11 test 9

#[test]
fn arity_and_option_validation() {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(&dir, "arity.db");
    let nt = write_fixture(
        &dir,
        "tiny.nt",
        "<http://e.com/a> <http://e.com/p> \"v\" .\n\
         <http://e.com/b> <http://e.com/p> \"w\" .\n",
    );

    // Head arity is checked at parse time against the fixed 6.
    let err = run_err(&db, &format!("?[s, p, o] <~ RdfReader(url: '{nt}')"));
    assert!(!err.is_empty());

    // prepend_index: arity 7, 0-based counter first (CsvReader parity).
    let rows = run(
        &db,
        &format!("?[i, s, p, o, g, lang, dt] <~ RdfReader(url: '{nt}', prepend_index: true)"),
    )
    .into_json()["rows"]
        .clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let mut idx: Vec<i64> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
    idx.sort_unstable();
    assert_eq!(idx, vec![0, 1]);

    // Unknown format name.
    let err = run_err(
        &db,
        &format!("?[s, p, o, g, lang, dt] <~ RdfReader(url: '{nt}', format: 'rdfxml')"),
    );
    assert!(err.contains("expected one of"), "unexpected error: {err}");

    // Undeterminable format: no option, no known extension.
    let unk = write_fixture(&dir, "data.rdf", "");
    let err = run_err(
        &db,
        &format!("?[s, p, o, g, lang, dt] <~ RdfReader(url: '{unk}')"),
    );
    assert!(
        err.contains("cannot determine the RDF format"),
        "unexpected error: {err}"
    );

    // Explicit format overrides a missing extension.
    let aliased = write_fixture(&dir, "noext", "<http://e.com/a> <http://e.com/p> \"v\" .\n");
    let rows = read_rows(&db, &aliased, ", format: 'ntriples'");
    assert_eq!(rows.len(), 1);
}

// §11 test 6 (the reachable half): with `rdf-io` on, the reader is registered.
// The iff-assertion across both feature states lives in
// tests/data_import_security.rs; the no-oxttl-symbols half is a feature-matrix
// compile check, not runnable from inside this gated file.
#[test]
fn rdf_reader_is_registered_under_rdf_io() {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();
    assert!(db.get_fixed_rules().contains_key("RdfReader"));
}

#[cfg(not(feature = "requests"))]
#[test]
fn http_url_without_requests_names_the_feature() {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();
    let err = run_err(
        &db,
        "?[s, p, o, g, lang, dt] <~ RdfReader(url: 'https://example.com/x.ttl')",
    );
    assert!(err.contains("requests"), "unexpected error: {err}");
}

// §11 test 8 (script-level half): the IRI helpers are reachable from
// CozoScript. The pure unit tests live in src/data/tests/functions.rs.
#[test]
fn iri_helpers_reachable_from_script() {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();
    let res = run(
        &db,
        "?[a, b, c, d] := a = iri_valid('http://x.com/'), \
         b = iri_resolve('http://x.com/a/', '../b'), \
         c = curie_expand(parse_json('{\"ex\": \"http://x.com/\"}'), 'ex:z'), \
         d = curie_compact(parse_json('{\"ex\": \"http://x.com/\"}'), 'http://x.com/z')",
    );
    assert_eq!(
        res.into_json()["rows"],
        serde_json::json!([[true, "http://x.com/b", "http://x.com/z", "ex:z"]])
    );
}

// ---------------------------------------------------------------- §11 test 2

/// Blank-node cells make row comparison label-sensitive; normalize them.
fn normalize_bnodes(mut rows: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    for row in &mut rows {
        for cell in row.as_array_mut().unwrap() {
            if let Some(s) = cell.as_str() {
                if s.starts_with("_:") {
                    *cell = serde_json::json!("_:");
                }
            }
        }
    }
    rows.sort_by_key(|r| r.to_string());
    rows
}

#[test]
fn round_trip_turtle_and_ntriples() {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(&dir, "roundtrip.db");
    let ttl = write_fixture(
        &dir,
        "rt.ttl",
        r#"@prefix ex: <http://example.com/> .
ex:a ex:name "Alice" .
ex:a ex:age "33"^^<http://www.w3.org/2001/XMLSchema#integer> .
ex:a ex:nick "Ali"@en .
ex:a ex:knows ex:b .
ex:a ex:note _:n1 .
_:n1 ex:text "a note" .
"#,
    );
    let original = read_rows(&db, &ttl, "");
    assert_eq!(original.len(), 6);

    run(
        &db,
        ":create triples {s: String, p: String, o: String, g: String?, lang: String?, dt: String?}",
    );
    run(
        &db,
        &format!(
            "?[s, p, o, g, lang, dt] <~ RdfReader(url: '{ttl}') :put triples {{s, p, o, g, lang, dt}}"
        ),
    );

    for format in ["turtle", "ntriples"] {
        let exported = db
            .export_relation_as_rdf("triples", format, &BTreeMap::new())
            .unwrap();
        let back = write_fixture(&dir, &format!("back.{format}"), &exported);
        let fmt_opt = format!(", format: '{format}'");
        let reread = read_rows(&db, &back, &fmt_opt);
        // Term-identical modulo blank-node labels (spec §11 test 2).
        assert_eq!(
            normalize_bnodes(original.clone()),
            normalize_bnodes(reread),
            "{format} round trip diverged"
        );
    }

    // With prefixes, the Turtle export compacts and still round-trips.
    let mut prefixes = BTreeMap::new();
    prefixes.insert("ex".to_string(), "http://example.com/".to_string());
    let exported = db
        .export_relation_as_rdf("triples", "turtle", &prefixes)
        .unwrap();
    assert!(exported.contains("@prefix ex:"));
    let back = write_fixture(&dir, "back_prefixed.ttl", &exported);
    assert_eq!(
        normalize_bnodes(original.clone()),
        normalize_bnodes(read_rows(&db, &back, ""))
    );
}

#[test]
fn round_trip_quads_and_skolemized_identity() {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(&dir, "roundtrip_q.db");
    let nq = write_fixture(
        &dir,
        "rt.nq",
        "<http://e.com/a> <http://e.com/p> \"v\" <http://e.com/g1> .\n\
         <http://e.com/b> <http://e.com/p> _:x <http://e.com/g1> .\n\
         <http://e.com/c> <http://e.com/p> \"w\"@en .\n",
    );
    // Skolemize on intake: the stored graph carries stable IRIs, so the round
    // trip is exactly term-identical, blank nodes included.
    let opt = ", skolemize: 'http://e.com/.well-known/genid/'";
    let original = read_rows(&db, &nq, opt);

    run(
        &db,
        ":create quads {s: String, p: String, o: String, g: String?, lang: String?, dt: String?}",
    );
    run(
        &db,
        &format!(
            "?[s, p, o, g, lang, dt] <~ RdfReader(url: '{nq}'{opt}) :put quads {{s, p, o, g, lang, dt}}"
        ),
    );
    for format in ["nquads", "trig"] {
        let exported = db
            .export_relation_as_rdf("quads", format, &BTreeMap::new())
            .unwrap();
        let back = write_fixture(&dir, &format!("back_q.{format}"), &exported);
        let fmt_opt = format!(", format: '{format}'");
        assert_eq!(
            original,
            read_rows(&db, &back, &fmt_opt),
            "{format} round trip must be exactly term-identical under skolemization"
        );
    }
}

// -------------------------------------------------- export strictness (Q5)

#[test]
fn export_rejects_wrong_shapes_and_formats() {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(&dir, "strict.db");

    // Not the 6-column shape: strict rejection.
    run(&db, ":create narrow {s: String, p: String, o: String}");
    let err = db
        .export_relation_as_rdf("narrow", "turtle", &BTreeMap::new())
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("6-column"),
        "unexpected error: {err:?}"
    );

    run(
        &db,
        ":create shaped {s: String, p: String, o: String, g: String?, lang: String?, dt: String?}",
    );
    run(
        &db,
        "?[s, p, o, g, lang, dt] <- [['http://e.com/a', 'http://e.com/p', 'v', 'http://e.com/g1', null, null]] \
         :put shaped {s, p, o, g, lang, dt}",
    );

    // A named graph cannot ride a triple format.
    let err = db
        .export_relation_as_rdf("shaped", "turtle", &BTreeMap::new())
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("names a graph"),
        "unexpected error: {err:?}"
    );
    // …but the quad formats carry it.
    let out = db
        .export_relation_as_rdf("shaped", "nquads", &BTreeMap::new())
        .unwrap();
    assert!(out.contains("<http://e.com/g1>"));

    // Unknown format; prefixes on a line format.
    let err = db
        .export_relation_as_rdf("shaped", "rdfxml", &BTreeMap::new())
        .unwrap_err();
    assert!(format!("{err:?}").contains("unknown RDF export format"));
    let mut prefixes = BTreeMap::new();
    prefixes.insert("ex".to_string(), "http://e.com/".to_string());
    let err = db
        .export_relation_as_rdf("shaped", "nquads", &prefixes)
        .unwrap_err();
    assert!(format!("{err:?}").contains("not applicable"));
}
