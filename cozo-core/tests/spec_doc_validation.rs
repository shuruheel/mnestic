/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Pins every ✓-marked (validated) code listing in `docs/specs/cozoscript-extensions.md`
//! against the real engine, so the spec's "works today, zero engine change" claims
//! cannot silently rot. Uses the sqlite backend per the repo test-backend rule
//! (the `mem` backend takes a different stored-join path).

use cozo::{DataValue, DbInstance, JsonData, ScriptMutability};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

#[test]
fn spec_listings_run_as_documented() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spec_validation.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap();

    let run = |script: &str, params: BTreeMap<String, DataValue>| {
        db.run_script(script, params, ScriptMutability::Mutable)
    };
    let run_ok = |script: &str, params: BTreeMap<String, DataValue>| {
        run(script, params).unwrap_or_else(|e| panic!("script failed: {script}\n{e:?}"))
    };
    let no_params = BTreeMap::new;

    // ---------- §3.2 corrected witness pattern (tt-only system-versioned witness) ----------
    run_ok(
        ":create complete_witness {rel: String, topic: String, tt: TxTime => note: String}",
        no_params(),
    );
    run_ok(":create claim {id: String => topic: String}", no_params());
    run_ok(
        ":create supports {src: String, dst: String => weight: Float}",
        no_params(),
    );

    // t0: an instant BEFORE the witness is asserted (for as-of reads later).
    let t0 = now_us();
    std::thread::sleep(std::time::Duration::from_millis(5));

    run_ok(
        r#"?[rel, topic, note] <- [["supports", "outage_2026_07", "all sources ingested"]]
           :put complete_witness {rel, topic => note}"#,
        no_params(),
    );
    run_ok(
        r#"?[id, topic] <- [["c1", "outage_2026_07"], ["c2", "unrelated_topic"], ["c3", "outage_2026_07"]]
           :put claim {id => topic}"#,
        no_params(),
    );
    run_ok(
        r#"?[src, dst, weight] <- [["s1", "c3", 0.9]]
           :put supports {src, dst => weight}"#,
        no_params(),
    );

    // The three-valued split, exactly as the spec documents it.
    let three_valued = r#"
        explained[c] := *claim[c, _], *supports[_, c, _]
        blocked[c]   := *claim[c, t], not explained[c],
                        *complete_witness{rel: "supports", topic: t}
        open[c]      := *claim[c, t], not explained[c],
                        not *complete_witness{rel: "supports", topic: t}
        ?[c, status] := explained[c], status = "explained"
        ?[c, status] := blocked[c], status = "blocked"
        ?[c, status] := open[c], status = "open"
    "#;
    let res = run_ok(three_valued, no_params());
    let mut rows: Vec<(String, String)> = res
        .rows
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_str().unwrap().to_string(),
            )
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("c1".into(), "blocked".into()),
            ("c2".into(), "open".into()),
            ("c3".into(), "explained".into()),
        ],
        "three-valued split must be explained/blocked/open"
    );

    // Per-atom as-of read of the witness: before t0 nothing was believed complete.
    let mut p = BTreeMap::new();
    p.insert("t0".to_string(), DataValue::from(t0));
    let res = run_ok(
        "?[rel, topic] := *complete_witness{rel, topic @ (tt: $t0)}",
        p.clone(),
    );
    assert_eq!(res.rows.len(), 0, "witness must be absent as of t0");

    // Whole-block :as_of — the same three-valued program pinned to t0: c1 flips to open.
    let three_valued_asof = format!("{three_valued}\n:as_of $t0");
    let res = run_ok(&three_valued_asof, p.clone());
    let mut rows: Vec<(String, String)> = res
        .rows
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_str().unwrap().to_string(),
            )
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("c1".into(), "open".into()),
            ("c2".into(), "open".into()),
            ("c3".into(), "explained".into()),
        ],
        ":as_of $t0 must flip c1 from blocked to open"
    );

    // ---------- §3.3 structured params: Json map param + get() + -> ----------
    let mut ctx_params = BTreeMap::new();
    ctx_params.insert(
        "ctx".to_string(),
        DataValue::Json(JsonData(serde_json::json!({
            "viewer": "auditor",
            "asof": t0,
        }))),
    );

    let res = run_ok("?[v] := v = get($ctx, 'viewer')", ctx_params.clone());
    assert_eq!(
        res.rows[0][0].get_str(),
        Some("auditor"),
        "get($ctx, 'viewer')"
    );

    let res = run_ok("?[v] := v = $ctx->'viewer'", ctx_params.clone());
    assert_eq!(res.rows[0][0].get_str(), Some("auditor"), "$ctx->'viewer'");

    // The trap: $ctx.viewer parses as a single param literally named "ctx.viewer".
    let err = run("?[v] := v = $ctx.viewer", ctx_params.clone());
    assert!(
        err.is_err(),
        "$ctx.viewer must fail (param named 'ctx.viewer' not supplied)"
    );

    // :as_of driven from a structured param field.
    let res = run_ok(
        "?[rel, topic] := *complete_witness{rel, topic}\n:as_of get($ctx, 'asof')",
        ctx_params.clone(),
    );
    assert_eq!(res.rows.len(), 0, ":as_of get($ctx, 'asof') must pin to t0");

    // ---------- §3.1/A.1/A.5 corrected aggregate calling conventions ----------
    // min_cost: pack bound in the body, [payload, cost]; returns the whole pack.
    let res = run_ok(
        r#"
        answer[cause, min_cost(pack)] := *supports[src, cause, w], pack = [src, -ln(w)]
        ?[cause, pack] := answer[cause, pack]
        "#,
        no_params(),
    );
    assert_eq!(res.rows.len(), 1);

    // min_cost_k: same body-bound pack convention, k as the trailing arg.
    let res = run_ok(
        r#"
        chain[cause, min_cost_k(pack, 3)] := *supports[src, cause, w],
                                             pack = [[src, cause], -ln(w)]
        ?[cause, pack] := chain[cause, pack]
        "#,
        no_params(),
    );
    assert_eq!(res.rows.len(), 1);

    // ---------- A.3: policy in the query + :as_of from a structured param ----------
    run_ok(
        ":create source {src: String, tt: TxTime => trust: Float}",
        no_params(),
    );
    run_ok(
        r#"?[src, trust] <- [["s1", 0.9], ["s2", 0.4]] :put source {src => trust}"#,
        no_params(),
    );
    let a3 = r#"
        visible[s] := *source{src: s, trust}, trust >= 0.7
        ?[c] := *supports[s, c, _], visible[s]
    "#;
    let res = run_ok(a3, no_params());
    assert_eq!(res.rows.len(), 1, "c3 is visible via trusted s1");
    let res = run_ok(
        &format!("{a3}\n:as_of get($ctx, 'asof')"),
        ctx_params.clone(),
    );
    assert_eq!(res.rows.len(), 0, "as of t0 no source belief existed");

    // ---------- A.5 composition: min_cost_k over a tt-stamped relation + :as_of ----------
    run_ok(
        ":create supports_b {src: String, cause: String, tt: TxTime => w: Float}",
        no_params(),
    );
    run_ok(
        r#"?[src, cause, w] <- [["s1", "power_fault", 0.9], ["s2", "power_fault", 0.6]]
           :put supports_b {src, cause => w}"#,
        no_params(),
    );
    std::thread::sleep(std::time::Duration::from_millis(5));
    let t1 = now_us();

    let a5 = r#"
        chain[cause, min_cost_k(pack, 3)] := *supports_b{src, cause, w},
                                             pack = [[src, cause], -ln(w)]
        ?[cause, pack] := chain[cause, pack]
    "#;
    let res = run_ok(a5, no_params());
    assert_eq!(
        res.rows.len(),
        2,
        "top-k proofs: both sources' packs survive at k=3"
    );

    // Reproducible as of t1 (both rows committed before t1)...
    let mut p1 = BTreeMap::new();
    p1.insert("t1".to_string(), DataValue::from(t1));
    let res = run_ok(&format!("{a5}\n:as_of $t1"), p1.clone());
    assert_eq!(res.rows.len(), 2, ":as_of t1 sees the committed beliefs");
    // ...and empty as of t0, before the relation had any belief.
    let res = run_ok(&format!("{a5}\n:as_of $t0"), p.clone());
    assert_eq!(
        res.rows.len(),
        0,
        ":as_of t0 predates every supports_b belief"
    );

    // ---------- §3.4 [start, end] list intervals are ordinary data today ----------
    run_ok(
        ":create fact_spans {entity: String, span_start: Int, span_end: Int => value: String}",
        no_params(),
    );
    run_ok(
        r#"?[entity, span_start, span_end, value] <- [
             ["e1", 0, 10, "v"], ["e1", 5, 15, "w"], ["e1", 20, 30, "x"]
           ] :put fact_spans {entity, span_start, span_end => value}"#,
        no_params(),
    );
    // Overlap as a plain predicate over list-shaped spans (what interval_overlaps would wrap).
    let res = run_ok(
        r#"
        ?[a, b] := *fact_spans{entity: e, span_start: s1, span_end: e1, value: a},
                   *fact_spans{entity: e, span_start: s2, span_end: e2, value: b},
                   a != b, s1 < e2, s2 < e1
        "#,
        no_params(),
    );
    assert_eq!(
        res.rows.len(),
        2,
        "v/w overlap symmetric; x overlaps nothing"
    );

    // ---------- §3.4 v1 primitives: interval_overlaps + interval_coalesce ----------
    // A.4's formerly-PROPOSED overlap block, now shipped: builtin agrees with the predicate.
    let res = run_ok(
        r#"
        conflict[a, b] := *fact_spans{entity: e, span_start: s1, span_end: e1, value: a},
                          *fact_spans{entity: e, span_start: s2, span_end: e2, value: b},
                          a != b, interval_overlaps([s1, e1], [s2, e2])
        ?[a, b] := conflict[a, b]
        "#,
        no_params(),
    );
    assert_eq!(res.rows.len(), 2, "builtin agrees with the plain predicate");

    // Half-open semantics: touching intervals do not overlap; mixed int/float bounds compare.
    let res = run_ok(
        "?[x] := x = interval_overlaps([0, 5], [5, 10])",
        no_params(),
    );
    assert_eq!(res.rows[0][0], DataValue::from(false));
    let res = run_ok(
        "?[x] := x = interval_overlaps([0, 5.5], [5, 10])",
        no_params(),
    );
    assert_eq!(res.rows[0][0], DataValue::from(true));
    // Mixed int/float at the exact boundary: bounds compare NUMERICALLY, not by
    // storage order (under Num's Ord, Int(5) < Float(5.0) — which would have
    // made this touching pair "overlap" depending on which side held the int).
    let res = run_ok(
        "?[x] := x = interval_overlaps([0, 5.0], [5, 10])",
        no_params(),
    );
    assert_eq!(res.rows[0][0], DataValue::from(false));
    let res = run_ok(
        "?[x] := x = interval_overlaps([0, 5], [5.0, 10])",
        no_params(),
    );
    assert_eq!(res.rows[0][0], DataValue::from(false));
    // An empty interval [x, x) contains no point: it overlaps nothing, even
    // when its point lies strictly inside the other span.
    let res = run_ok(
        "?[x] := x = interval_overlaps([5, 5], [0, 10])",
        no_params(),
    );
    assert_eq!(res.rows[0][0], DataValue::from(false));
    // Malformed spans are loud errors, not silent falses.
    assert!(run(
        "?[x] := x = interval_overlaps([5, 0], [0, 10])",
        no_params()
    )
    .is_err());
    assert!(run("?[x] := x = interval_overlaps(3, [0, 10])", no_params()).is_err());

    // A.4's formerly-PROPOSED coalesce block: fragmented equal-valued spans merge
    // into maximal intervals (adjacent half-open spans fuse; gaps stay separate).
    run_ok(
        r#"?[entity, span_start, span_end, value] <- [
             ["e2", 0, 5, "y"], ["e2", 5, 10, "y"], ["e2", 12, 20, "y"]
           ] :put fact_spans {entity, span_start, span_end => value}"#,
        no_params(),
    );
    let res = run_ok(
        r#"
        held[e, v, interval_coalesce(span)] :=
            *fact_spans{entity: e, span_start: s, span_end: t, value: v},
            span = [s, t]
        ?[e, v, spans] := held[e, v, spans], e = "e2"
        "#,
        no_params(),
    );
    assert_eq!(res.rows.len(), 1);
    let expected = DataValue::List(vec![
        DataValue::List(vec![DataValue::from(0i64), DataValue::from(10i64)]),
        DataValue::List(vec![DataValue::from(12i64), DataValue::from(20i64)]),
    ]);
    assert_eq!(res.rows[0][2], expected, "[0,5)+[5,10) fuse; [12,20) stays");

    // Malformed spans error loudly in the aggregate too.
    assert!(run(
        r#"
        bad[e, interval_coalesce(span)] :=
            *fact_spans{entity: e, span_start: s, span_end: t}, span = [t, s]
        ?[e, x] := bad[e, x]
        "#,
        no_params(),
    )
    .is_err());
}
