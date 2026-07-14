/*
 *  Copyright 2022, The Cozo Project Authors.
 *
 *  This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 *  If a copy of the MPL was not distributed with this file,
 *  You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 */

use std::collections::BTreeMap;
use std::time::Duration;

use itertools::Itertools;
use log::debug;
use serde_json::json;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::FixedRulePayload;
use crate::fts::{TokenizerCache, TokenizerConfig};
use crate::parse::SourceSpan;
use crate::runtime::callback::CallbackOp;
use crate::runtime::db::Poison;
use crate::{DbInstance, FixedRule, RegularTempStore, ScriptMutability};

#[test]
fn test_limit_offset() {
    let db = DbInstance::default();
    let res = db
        .run_default("?[a] := a in [5,3,1,2,4] :limit 2")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], json!([[3], [5]]));
    let res = db
        .run_default("?[a] := a in [5,3,1,2,4] :limit 2 :offset 1")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], json!([[1], [3]]));
    let res = db
        .run_default("?[a] := a in [5,3,1,2,4] :limit 2 :offset 4")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], json!([[4]]));
    let res = db
        .run_default("?[a] := a in [5,3,1,2,4] :limit 2 :offset 5")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], json!([]));
}

#[test]
fn test_normal_aggr_empty() {
    let db = DbInstance::default();
    let res = db.run_default("?[count(a)] := a in []").unwrap().rows;
    assert_eq!(res, vec![vec![DataValue::from(0)]]);
}

#[test]
fn test_meet_aggr_empty() {
    let db = DbInstance::default();
    let res = db.run_default("?[min(a)] := a in []").unwrap().rows;
    assert_eq!(res, vec![vec![DataValue::Null]]);

    let res = db
        .run_default("?[min(a), count(a)] := a in []")
        .unwrap()
        .rows;
    assert_eq!(res, vec![vec![DataValue::Null, DataValue::from(0)]]);
}

#[test]
fn test_layers() {
    let _ = env_logger::builder().is_test(true).try_init();

    let db = DbInstance::default();
    let res = db
        .run_default(
            r#"
        y[a] := a in [1,2,3]
        x[sum(a)] := y[a]
        x[sum(a)] := a in [4,5,6]
        ?[sum(a)] := x[a]
        "#,
        )
        .unwrap()
        .rows;
    assert_eq!(res[0][0], DataValue::from(21.))
}

#[test]
fn test_conditions() {
    let _ = env_logger::builder().is_test(true).try_init();
    let db = DbInstance::default();
    db.run_default(
        r#"
        {
            ?[code] <- [['a'],['b'],['c']]
            :create airport {code}
        }
        {
            ?[fr, to, dist] <- [['a', 'b', 1.1], ['a', 'c', 0.5], ['b', 'c', 9.1]]
            :create route {fr, to => dist}
        }
        "#,
    )
    .unwrap();
    debug!("real test begins");
    let res = db
        .run_default(
            r#"
        r[code, dist] := *airport{code}, *route{fr: code, dist};
        ?[dist] := r['a', dist], dist > 0.5, dist <= 1.1;
        "#,
        )
        .unwrap()
        .rows;
    assert_eq!(res[0][0], DataValue::from(1.1))
}

#[test]
fn test_classical() {
    let _ = env_logger::builder().is_test(true).try_init();
    let db = DbInstance::default();
    let res = db
        .run_default(
            r#"
parent[] <- [['joseph', 'jakob'],
             ['jakob', 'isaac'],
             ['isaac', 'abraham']]
grandparent[gcld, gp] := parent[gcld, p], parent[p, gp]
?[who] := grandparent[who, 'abraham']
        "#,
        )
        .unwrap()
        .rows;
    println!("{:?}", res);
    assert_eq!(res[0][0], DataValue::from("jakob"))
}

#[test]
fn default_columns() {
    let db = DbInstance::default();

    db.run_default(
        r#"
            :create status {uid: String, ts default now() => quitted: Bool, mood: String}
            "#,
    )
    .unwrap();

    db.run_default(
        r#"
        ?[uid, quitted, mood] <- [['z', true, 'x']]
            :put status {uid => quitted, mood}
        "#,
    )
    .unwrap();
}

#[test]
fn rm_does_not_need_all_keys() {
    let db = DbInstance::default();
    db.run_default(":create status {uid => mood}").unwrap();
    assert!(db
        .run_default("?[uid, mood] <- [[1, 2]] :put status {uid => mood}",)
        .is_ok());
    assert!(db
        .run_default("?[uid, mood] <- [[2]] :put status {uid}",)
        .is_err());
    assert!(db
        .run_default("?[uid, mood] <- [[3, 2]] :rm status {uid => mood}",)
        .is_ok());
    assert!(db.run_default("?[uid] <- [[1]] :rm status {uid}").is_ok());
}

#[test]
fn strict_checks_for_fixed_rules_args() {
    let db = DbInstance::default();
    let res = db.run_default(
        r#"
            r[] <- [[1, 2]]
            ?[] <~ PageRank(r[_, _])
        "#,
    );
    println!("{:?}", res);
    assert!(res.is_ok());

    let db = DbInstance::default();
    let res = db.run_default(
        r#"
            r[] <- [[1, 2]]
            ?[] <~ PageRank(r[a, b])
        "#,
    );
    assert!(res.is_ok());

    let db = DbInstance::default();
    let res = db.run_default(
        r#"
            r[] <- [[1, 2]]
            ?[] <~ PageRank(r[a, a])
        "#,
    );
    assert!(res.is_err());
}

#[test]
fn do_not_unify_underscore() {
    let db = DbInstance::default();
    let res = db
        .run_default(
            r#"
        r1[] <- [[1, 'a'], [2, 'b']]
        r2[] <- [[2, 'B'], [3, 'C']]

        ?[l1, l2] := r1[_ , l1], r2[_ , l2]
        "#,
        )
        .unwrap()
        .rows;
    assert_eq!(res.len(), 4);

    let res = db.run_default(
        r#"
        ?[_] := _ = 1
        "#,
    );
    assert!(res.is_err());

    let res = db
        .run_default(
            r#"
        ?[x] := x = 1, _ = 1, _ = 2
        "#,
        )
        .unwrap()
        .rows;

    assert_eq!(res.len(), 1);
}

#[test]
fn imperative_script() {
    // let db = DbInstance::default();
    // let res = db
    //     .run_default(
    //         r#"
    //     {:create _test {a}}
    //
    //     %loop
    //         %if { len[count(x)] := *_test[x]; ?[x] := len[z], x = z >= 10 }
    //             %then %return _test
    //         %end
    //         { ?[a] := a = rand_uuid_v1(); :put _test {a} }
    //         %debug _test
    //     %end
    // "#,
    //         Default::default(),
    //     )
    //     .unwrap();
    // assert_eq!(res.rows.len(), 10);
    //
    // let res = db
    //     .run_default(
    //         r#"
    //     {?[a] <- [[1], [2], [3]]
    //      :replace _test {a}}
    //
    //     %loop
    //         { ?[a] := *_test[a]; :limit 1; :rm _test {a} }
    //         %debug _test
    //
    //         %if_not _test
    //         %then %break
    //         %end
    //     %end
    //
    //     %return _test
    // "#,
    //         Default::default(),
    //     )
    //     .unwrap();
    // assert_eq!(res.rows.len(), 0);
    //
    // let res = db.run_default(
    //     r#"
    //     {:create _test {a}}
    //
    //     %loop
    //         { ?[a] := a = rand_uuid_v1(); :put _test {a} }
    //
    //         %if { len[count(x)] := *_test[x]; ?[x] := len[z], x = z < 10 }
    //             %continue
    //         %end
    //
    //         %return _test
    //         %debug _test
    //     %end
    // "#,
    //     Default::default(),
    // );
    // if let Err(err) = &res {
    //     eprintln!("{err:?}");
    // }
    // assert_eq!(res.unwrap().rows.len(), 10);
    //
    // let res = db
    //     .run_default(
    //         r#"
    //     {?[a] <- [[1], [2], [3]]
    //      :replace _test {a}}
    //     {?[a] <- []
    //      :replace _test2 {a}}
    //     %swap _test _test2
    //     %return _test
    // "#,
    //         Default::default(),
    //     )
    //     .unwrap();
    // assert_eq!(res.rows.len(), 0);
}

#[test]
fn returning_relations() {
    let db = DbInstance::default();
    let res = db
        .run_default(
            r#"
        {:create _xxz {a}}
        {?[a] := a in [5,4,1,2,3] :put _xxz {a}}
        {?[a] := *_xxz[a], a % 2 == 0 :rm _xxz {a}}
        {?[a] := *_xxz[b], a = b * 2}
        "#,
        )
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[2], [6], [10]]));
    let res = db.run_default(
        r#"
        {?[a] := *_xxz[b], a = b * 2}
        "#,
    );
    assert!(res.is_err());
}

#[test]
fn test_trigger() {
    let db = DbInstance::default();
    db.run_default(":create friends {fr: Int, to: Int => data: Any}")
        .unwrap();
    db.run_default(":create friends.rev {to: Int, fr: Int => data: Any}")
        .unwrap();
    db.run_default(
        r#"
        ::set_triggers friends

        on put {
            ?[fr, to, data] := _new[fr, to, data]

            :put friends.rev{ to, fr => data}
        }
        on rm {
            ?[fr, to] := _old[fr, to, data]

            :rm friends.rev{ to, fr }
        }
        "#,
    )
    .unwrap();
    db.run_default(r"?[fr, to, data] <- [[1,2,3]] :put friends {fr, to => data}")
        .unwrap();
    let ret = db
        .export_relations(["friends", "friends.rev"].into_iter())
        .unwrap();
    let frs = ret.get("friends").unwrap();
    assert_eq!(
        vec![DataValue::from(1), DataValue::from(2), DataValue::from(3)],
        frs.rows[0]
    );

    let frs_rev = ret.get("friends.rev").unwrap();
    assert_eq!(
        vec![DataValue::from(2), DataValue::from(1), DataValue::from(3)],
        frs_rev.rows[0]
    );
    db.run_default(r"?[fr, to] <- [[1,2], [2,3]] :rm friends {fr, to}")
        .unwrap();
    let ret = db
        .export_relations(["friends", "friends.rev"].into_iter())
        .unwrap();
    let frs = ret.get("friends").unwrap();
    assert!(frs.rows.is_empty());
}

#[test]
fn test_callback() {
    let db = DbInstance::default();
    let mut collected = vec![];
    let (_id, receiver) = db.register_callback("friends", None);
    db.run_default(":create friends {fr: Int, to: Int => data: Any}")
        .unwrap();
    db.run_default(r"?[fr, to, data] <- [[1,2,3],[4,5,6]] :put friends {fr, to => data}")
        .unwrap();
    db.run_default(r"?[fr, to, data] <- [[1,2,4],[4,7,6]] :put friends {fr, to => data}")
        .unwrap();
    db.run_default(r"?[fr, to] <- [[1,9],[4,5]] :rm friends {fr, to}")
        .unwrap();
    std::thread::sleep(Duration::from_secs_f64(0.01));
    while let Ok(d) = receiver.try_recv() {
        collected.push(d);
    }
    let collected = collected;
    assert_eq!(collected[0].0, CallbackOp::Put);
    assert_eq!(collected[0].1.rows.len(), 2);
    assert_eq!(collected[0].1.rows[0].len(), 3);
    assert_eq!(collected[0].2.rows.len(), 0);
    assert_eq!(collected[1].0, CallbackOp::Put);
    assert_eq!(collected[1].1.rows.len(), 2);
    assert_eq!(collected[1].1.rows[0].len(), 3);
    assert_eq!(collected[1].2.rows.len(), 1);
    assert_eq!(
        collected[1].2.rows[0],
        vec![DataValue::from(1), DataValue::from(2), DataValue::from(3)]
    );
    assert_eq!(collected[2].0, CallbackOp::Rm);
    assert_eq!(collected[2].1.rows.len(), 2);
    assert_eq!(collected[2].1.rows[0].len(), 2);
    assert_eq!(collected[2].2.rows.len(), 1);
    assert_eq!(collected[2].2.rows[0].len(), 3);
}

#[test]
fn test_update() {
    let db = DbInstance::default();
    db.run_default(":create friends {fr: Int, to: Int => a: Any, b: Any, c: Any}")
        .unwrap();
    db.run_default("?[fr, to, a, b, c] <- [[1,2,3,4,5]] :put friends {fr, to => a, b, c}")
        .unwrap();
    let res = db
        .run_default("?[fr, to, a, b, c] := *friends{fr, to, a, b, c}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0], json!([1, 2, 3, 4, 5]));
    db.run_default("?[fr, to, b] <- [[1, 2, 100]] :update friends {fr, to => b}")
        .unwrap();
    let res = db
        .run_default("?[fr, to, a, b, c] := *friends{fr, to, a, b, c}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0], json!([1, 2, 3, 100, 5]));
}

#[test]
fn test_index() {
    let db = DbInstance::default();
    db.run_default(":create friends {fr: Int, to: Int => data: Any}")
        .unwrap();

    db.run_default(r"?[fr, to, data] <- [[1,2,3],[4,5,6]] :put friends {fr, to, data}")
        .unwrap();

    assert!(db
        .run_default("::index create friends:rev {to, no}")
        .is_err());
    db.run_default("::index create friends:rev {to, data}")
        .unwrap();

    db.run_default(r"?[fr, to, data] <- [[1,2,5],[6,5,7]] :put friends {fr, to => data}")
        .unwrap();
    db.run_default(r"?[fr, to] <- [[4,5]] :rm friends {fr, to}")
        .unwrap();

    let rels_data = db
        .export_relations(["friends", "friends:rev"].into_iter())
        .unwrap();
    assert_eq!(
        rels_data["friends"].clone().into_json()["rows"],
        json!([[1, 2, 5], [6, 5, 7]])
    );
    assert_eq!(
        rels_data["friends:rev"].clone().into_json()["rows"],
        json!([[2, 5, 1], [5, 7, 6]])
    );

    let rels = db.run_default("::relations").unwrap();
    assert_eq!(rels.rows[1][0], DataValue::from("friends:rev"));
    assert_eq!(rels.rows[1][1], DataValue::from(3));
    assert_eq!(rels.rows[1][2], DataValue::from("index"));

    let cols = db.run_default("::columns friends:rev").unwrap();
    assert_eq!(cols.rows.len(), 3);

    let res = db
        .run_default("?[fr, data] := *friends:rev{to: 2, fr, data}")
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1, 5]]));

    let res = db
        .run_default("?[fr, data] := *friends{to: 2, fr, data}")
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1, 5]]));

    let expl = db
        .run_default("::explain { ?[fr, data] := *friends{to: 2, fr, data} }")
        .unwrap();
    let joins = expl.into_json()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row.as_array().unwrap()[5].clone())
        .collect_vec();
    assert!(joins.contains(&json!(":friends:rev")));
    db.run_default("::index drop friends:rev").unwrap();
}

#[test]
fn test_json_objects() {
    let db = DbInstance::default();
    db.run_default("?[a] := a = {'a': 1}").unwrap();
    db.run_default(
        r"?[a] := a = {
            'a': 1
        }",
    )
    .unwrap();
}

#[test]
fn test_custom_rules() {
    let db = DbInstance::default();
    struct Custom;

    impl FixedRule for Custom {
        fn arity(
            &self,
            _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
            _rule_head: &[Symbol],
            _span: SourceSpan,
        ) -> miette::Result<usize> {
            Ok(1)
        }

        fn run(
            &self,
            payload: FixedRulePayload<'_, '_>,
            out: &'_ mut RegularTempStore,
            _poison: Poison,
        ) -> miette::Result<()> {
            let rel = payload.get_input(0)?;
            let mult = payload.integer_option("mult", Some(2))?;
            for maybe_row in rel.iter()? {
                let row = maybe_row?;
                let mut sum = 0;
                for col in row {
                    let d = col.get_int().unwrap_or(0);
                    sum += d;
                }
                sum *= mult;
                out.put(vec![DataValue::from(sum)])
            }
            Ok(())
        }
    }

    db.register_fixed_rule("SumCols".to_string(), Custom)
        .unwrap();
    let res = db
        .run_default(
            r#"
        rel[] <- [[1,2,3,4],[5,6,7,8]]
        ?[x] <~ SumCols(rel[], mult: 100)
    "#,
        )
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1000], [2600]]));
}

#[test]
fn test_index_short() {
    let db = DbInstance::default();
    db.run_default(":create friends {fr: Int, to: Int => data: Any}")
        .unwrap();

    db.run_default(r"?[fr, to, data] <- [[1,2,3],[4,5,6]] :put friends {fr, to => data}")
        .unwrap();

    db.run_default("::index create friends:rev {to}").unwrap();

    db.run_default(r"?[fr, to, data] <- [[1,2,5],[6,5,7]] :put friends {fr, to => data}")
        .unwrap();
    db.run_default(r"?[fr, to] <- [[4,5]] :rm friends {fr, to}")
        .unwrap();

    let rels_data = db
        .export_relations(["friends", "friends:rev"].into_iter())
        .unwrap();
    assert_eq!(
        rels_data["friends"].clone().into_json()["rows"],
        json!([[1, 2, 5], [6, 5, 7]])
    );
    assert_eq!(
        rels_data["friends:rev"].clone().into_json()["rows"],
        json!([[2, 1], [5, 6]])
    );

    let rels = db.run_default("::relations").unwrap();
    assert_eq!(rels.rows[1][0], DataValue::from("friends:rev"));
    assert_eq!(rels.rows[1][1], DataValue::from(2));
    assert_eq!(rels.rows[1][2], DataValue::from("index"));

    let cols = db.run_default("::columns friends:rev").unwrap();
    assert_eq!(cols.rows.len(), 2);

    let expl = db
        .run_default("::explain { ?[fr, data] := *friends{to: 2, fr, data} }")
        .unwrap()
        .into_json();

    for row in expl["rows"].as_array().unwrap() {
        println!("{}", row);
    }

    let joins = expl["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row.as_array().unwrap()[5].clone())
        .collect_vec();
    assert!(joins.contains(&json!(":friends:rev")));

    let res = db
        .run_default("?[fr, data] := *friends{to: 2, fr, data}")
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1, 5]]));
}

#[test]
fn test_multi_tx() {
    let db = DbInstance::default();
    let tx = db.multi_transaction(true);
    tx.run_script(":create a {a}", Default::default()).unwrap();
    tx.run_script("?[a] <- [[1]] :put a {a}", Default::default())
        .unwrap();
    assert!(tx.run_script(":create a {a}", Default::default()).is_err());
    tx.run_script("?[a] <- [[2]] :put a {a}", Default::default())
        .unwrap();
    tx.run_script("?[a] <- [[3]] :put a {a}", Default::default())
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        db.run_default("?[a] := *a[a]").unwrap().into_json()["rows"],
        json!([[1], [2], [3]])
    );

    let db = DbInstance::default();
    let tx = db.multi_transaction(true);
    tx.run_script(":create a {a}", Default::default()).unwrap();
    tx.run_script("?[a] <- [[1]] :put a {a}", Default::default())
        .unwrap();
    assert!(tx.run_script(":create a {a}", Default::default()).is_err());
    tx.run_script("?[a] <- [[2]] :put a {a}", Default::default())
        .unwrap();
    tx.run_script("?[a] <- [[3]] :put a {a}", Default::default())
        .unwrap();
    tx.abort().unwrap();
    assert!(db.run_default("?[a] := *a[a]").is_err());
}

#[test]
fn test_vec_types() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a {k: String => v: <F32; 8>}")
        .unwrap();
    db.run_default("?[k, v] <- [['k', [1,2,3,4,5,6,7,8]]] :put a {k => v}")
        .unwrap();
    let res = db.run_default("?[k, v] := *a{k, v}").unwrap();
    assert_eq!(
        json!([1., 2., 3., 4., 5., 6., 7., 8.]),
        res.into_json()["rows"][0][1]
    );
    let res = db
        .run_default("?[v] <- [[vec([1,2,3,4,5,6,7,8])]]")
        .unwrap();
    assert_eq!(
        json!([1., 2., 3., 4., 5., 6., 7., 8.]),
        res.into_json()["rows"][0][0]
    );
    let res = db.run_default("?[v] <- [[rand_vec(5)]]").unwrap();
    assert_eq!(5, res.into_json()["rows"][0][0].as_array().unwrap().len());
    let res = db
        .run_default(r#"
            val[v] <- [[vec([1,2,3,4,5,6,7,8])]]
            ?[x,y,z] := val[v], x=l2_dist(v, v), y=cos_dist(v, v), nv = l2_normalize(v), z=ip_dist(nv, nv)
        "#)
        .unwrap();
    println!("{}", res.into_json());
}

#[test]
fn test_vec_index_insertion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(
        r"
        ?[k, v, m] <- [['a', [1,2], true],
                       ['b', [2,3], false]]

        :create a {k: String => v: <F32; 2>, m: Bool}
    ",
    )
    .unwrap();
    db.run_default(
        r"
        ::hnsw create a:vec {
            dim: 2,
            m: 50,
            dtype: F32,
            fields: [v],
            distance: L2,
            ef_construction: 20,
            filter: m,
            #extend_candidates: true,
            #keep_pruned_connections: true,
        }",
    )
    .unwrap();
    let res = db
        .run_default("?[k] := *a:vec{layer: 0, fr_k, to_k}, k = fr_k or k = to_k")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    println!("update!");
    db.run_default(r#"?[k, m] <- [["a", false]] :update a {}"#)
        .unwrap();
    let res = db
        .run_default("?[k] := *a:vec{layer: 0, fr_k, to_k}, k = fr_k or k = to_k")
        .unwrap();
    assert_eq!(res.rows.len(), 0);
    println!("{}", res.into_json());
}

#[test]
fn test_vec_index() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(
        r"
        ?[k, v] <- [['a', [1,2]],
                    ['b', [2,3]],
                    ['bb', [2,3]],
                    ['c', [3,4]],
                    ['x', [0,0.1]],
                    ['a', [112,0]],
                    ['b', [1,1]]]

        :create a {k: String => v: <F32; 2>}
    ",
    )
    .unwrap();
    db.run_default(
        r"
        ::hnsw create a:vec {
            dim: 2,
            m: 50,
            dtype: F32,
            fields: [v],
            distance: L2,
            ef_construction: 20,
            filter: k != 'k1',
            #extend_candidates: true,
            #keep_pruned_connections: true,
        }",
    )
    .unwrap();
    db.run_default(
        r"
        ?[k, v] <- [
                    ['a2', [1,25]],
                    ['b2', [2,34]],
                    ['bb2', [2,33]],
                    ['c2', [2,32]],
                    ['a2', [2,31]],
                    ['b2', [1,10]]
                    ]
        :put a {k => v}
        ",
    )
    .unwrap();

    println!("all links");
    for (_, nrows) in db.export_relations(["a:vec"].iter()).unwrap() {
        let nrows = nrows.rows;
        for row in nrows {
            println!("{} {} -> {} {}", row[0], row[1], row[4], row[7]);
        }
    }

    let res = db
        .run_default(
            r"
        #::explain {
        ?[dist, k, v] := ~a:vec{k, v | query: q, k: 2, ef: 20, bind_distance: dist}, q = vec([200, 34])
        #}
        ",
        )
        .unwrap();
    println!("results");
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{} {} {}", row[0], row[1], row[2]);
    }
}

#[test]
fn test_fts_indexing() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {k: String => v: String}")
        .unwrap();
    db.run_default(
        r"?[k, v] <- [['a', 'hello world!'], ['b', 'the world is round']] :put a {k => v}",
    )
    .unwrap();
    db.run_default(
        r"::fts create a:fts {
            extractor: v,
            tokenizer: Simple,
            filters: [Lowercase, Stemmer('English'), Stopwords('en')]
        }",
    )
    .unwrap();
    db.run_default(
        r"?[k, v] <- [
            ['b', 'the world is square!'],
            ['c', 'see you at the end of the world!'],
            ['d', 'the world is the world and makes the world go around']
        ] :put a {k => v}",
    )
    .unwrap();
    let res = db
        .run_default(
            r"
        ?[word, src_k, offset_from, offset_to, position, total_length] :=
            *a:fts{word, src_k, offset_from, offset_to, position, total_length}
        ",
        )
        .unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    println!("query");
    let res = db
        .run_default(r"?[k, v, s] := ~a:fts{k, v | query: 'world', k: 2, bind_score: s}")
        .unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
}

#[test]
fn test_lsh_indexing2() {
    for i in 1..10 {
        let f = i as f64 / 10.;
        let db = DbInstance::new("mem", "", "").unwrap();
        db.run_default(r":create a {k: String => v: String}")
            .unwrap();
        db.run_script(
            r"::lsh create a:lsh {extractor: v, tokenizer: NGram, n_gram: 3, target_threshold: $t }",
            BTreeMap::from([("t".into(), f.into())]),
            ScriptMutability::Mutable
        )
            .unwrap();
        db.run_default("?[k, v] <- [['a', 'ewiygfspeoighjsfcfxzdfncalsdf']] :put a {k => v}")
            .unwrap();
        let res = db
            .run_default("?[k] := ~a:lsh{k | query: 'ewiygfspeoighjsfcfxzdfncalsdf', k: 1}")
            .unwrap();
        assert!(res.rows.len() > 0);
    }
}

#[test]
fn test_lsh_indexing3() {
    for i in 1..10 {
        let f = i as f64 / 10.;
        let db = DbInstance::new("mem", "", "").unwrap();
        db.run_default(r":create text {id: String,  => text: String, url: String? default null, dt: Float default now(), dup_for: String? default null }")
            .unwrap();
        db.run_script(
            r"::lsh create text:lsh {
                    extractor: text,
                    # extract_filter: is_null(dup_for),
                    tokenizer: NGram,
                    n_perm: 200,
                    target_threshold: $t,
                    n_gram: 7,
                }",
            BTreeMap::from([("t".into(), f.into())]),
            ScriptMutability::Mutable,
        )
        .unwrap();
        db.run_default(
            "?[id, text] <- [['a', 'This function first generates 32 random bytes using the os.urandom function. It then base64 encodes these bytes using base64.urlsafe_b64encode, removes the padding, and decodes the result to a string.']] :put text {id, text}",
        )
        .unwrap();
        let res = db
            .run_default(
                r#"?[id, dup_for] :=
    ~text:lsh{id: id, dup_for: dup_for, | query: "This function first generates 32 random bytes using the os.urandom function. It then base64 encodes these bytes using base64.urlsafe_b64encode, removes the padding, and decodes the result to a string.", }"#,
            )
            .unwrap();
        assert!(res.rows.len() > 0);
        println!("{}", res.into_json());
    }
}

#[test]
fn filtering() {
    let db = DbInstance::default();
    let res = db
        .run_default(
            r"
        {
            ?[x, y] <- [[1, 2]]
            :create _rel {x => y}
            :returning
        }
        {
            ?[x, y] := x = 1, *_rel{x, y: 3}, y = 2
        }
    ",
        )
        .unwrap();
    assert_eq!(0, res.rows.len());

    let res = db
        .run_default(
            r"
        {
            ?[x, u, y] <- [[1, 0, 2]]
            :create _rel {x, u => y}
            :returning
        }
        {
            ?[x, y] := x = 1, *_rel{x, y: 3}, y = 2
        }
    ",
        )
        .unwrap();
    assert_eq!(0, res.rows.len());
}

#[test]
fn test_lsh_indexing4() {
    for i in 1..10 {
        let f = i as f64 / 10.;
        let db = DbInstance::new("mem", "", "").unwrap();
        db.run_default(r":create a {k: String => v: String}")
            .unwrap();
        db.run_script(
            r"::lsh create a:lsh {extractor: v, tokenizer: NGram, n_gram: 3, target_threshold: $t }",
            BTreeMap::from([("t".into(), f.into())]),
            ScriptMutability::Mutable
        )
            .unwrap();
        db.run_default("?[k, v] <- [['a', 'ewiygfspeoighjsfcfxzdfncalsdf']] :put a {k => v}")
            .unwrap();
        db.run_default("?[k] <- [['a']] :rm a {k}").unwrap();
        let res = db
            .run_default("?[k] := ~a:lsh{k | query: 'ewiygfspeoighjsfcfxzdfncalsdf', k: 1}")
            .unwrap();
        assert!(res.rows.len() == 0);
    }
}

#[test]
fn test_lsh_indexing() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {k: String => v: String}")
        .unwrap();
    db.run_default(
        r"?[k, v] <- [['a', 'hello world!'], ['b', 'the world is round']] :put a {k => v}",
    )
    .unwrap();
    db.run_default(
        r"::lsh create a:lsh {extractor: v, tokenizer: Simple, n_gram: 3, target_threshold: 0.3 }",
    )
    .unwrap();
    db.run_default(
        r"?[k, v] <- [
            ['b', 'the world is square!'],
            ['c', 'see you at the end of the world!'],
            ['d', 'the world is the world and makes the world go around'],
            ['e', 'the world is the world and makes the world not go around']
        ] :put a {k => v}",
    )
    .unwrap();
    let res = db.run_default("::columns a:lsh").unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    let _res = db
        .run_default(
            r"
        ?[src_k, hash] :=
            *a:lsh{src_k, hash}
        ",
        )
        .unwrap();
    // for row in _res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }
    let _res = db
        .run_default(
            r"
        ?[k, minhash] :=
            *a:lsh:inv{k, minhash}
        ",
        )
        .unwrap();
    // for row in res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }
    let res = db
        .run_default(
            r"
            ?[k, v] := ~a:lsh{k, v |
                query: 'see him at the end of the world',
            }
            ",
        )
        .unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    let res = db.run_default("::indices a").unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    db.run_default(r"::lsh drop a:lsh").unwrap();
}

#[test]
fn test_insertions() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {k => v: <F32; 1536> default rand_vec(1536)}")
        .unwrap();
    db.run_default(r"?[k] <- [[1]] :put a {k}").unwrap();
    db.run_default(r"?[k, v] := *a{k, v}").unwrap();
    db.run_default(
        r"::hnsw create a:i {
            fields: [v], dim: 1536, ef: 16, filter: k % 3 == 0,
            m: 32
        }",
    )
    .unwrap();
    db.run_default(r"?[count(fr_k)] := *a:i{fr_k}").unwrap();
    db.run_default(r"?[k] <- [[1]] :put a {k}").unwrap();
    db.run_default(r"?[k] := k in int_range(300) :put a {k}")
        .unwrap();
    let res = db
        .run_default(
            r"?[dist, k] := ~a:i{k | query: v, bind_distance: dist, k:10, ef: 50, filter: k % 2 == 0, radius: 245}, *a{k: 96, v}",
        )
        .unwrap();
    println!("results");
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{} {}", row[0], row[1]);
    }
}

#[test]
fn tokenizers() {
    let tokenizers = TokenizerCache::default();
    let tokenizer = tokenizers
        .get(
            "simple",
            &TokenizerConfig {
                name: "Simple".into(),
                args: vec![],
            },
            &[],
        )
        .unwrap();

    // let tokenizer = TextAnalyzer::from(SimpleTokenizer)
    //     .filter(RemoveLongFilter::limit(40))
    //     .filter(LowerCaser)
    //     .filter(Stemmer::new(Language::English));
    let mut token_stream = tokenizer.token_stream("It is closer to Apache Lucene than to Elasticsearch or Apache Solr in the sense it is not an off-the-shelf search engine server, but rather a crate that can be used to build such a search engine.");
    while let Some(token) = token_stream.next() {
        println!("Token {:?}", token.text);
    }

    println!("XXXXXXXXXXXXX");

    let tokenizer = tokenizers
        .get(
            "cangjie",
            &TokenizerConfig {
                name: "Cangjie".into(),
                args: vec![],
            },
            &[],
        )
        .unwrap();

    let mut token_stream = tokenizer.token_stream("这个产品Finchat.io是一个相对比较有特色的文档问答类网站，它集成了750多家公司的经融数据。感觉是把财报等数据借助Embedding都向量化了，然后接入ChatGPT进行对话。");
    while let Some(token) = token_stream.next() {
        println!("Token {:?}", token.text);
    }
}

#[test]
fn multi_index_vec() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(
        r#"
        :create product {
            id
            =>
            name,
            description,
            price,
            name_vec: <F32; 1>,
            description_vec: <F32; 1>
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::hnsw create product:semantic{
            fields: [name_vec, description_vec],
            dim: 1,
            ef: 16,
            m: 32,
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ?[id, name, description, price, name_vec, description_vec] <- [[1, "name", "description", 100, [1], [1]]]

        :put product {id => name, description, price, name_vec, description_vec}
        "#,
    ).unwrap();
    let res = db.run_default("::indices product").unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
}

#[test]
fn ensure_not() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(
        r"
    %ignore_error { :create id_alloc{id: Int => next_id: Int, last_id: Int}}
%ignore_error {
    ?[id, next_id, last_id] <- [[0, 1, 1000]];
    :ensure_not id_alloc{id => next_id, last_id}
}
    ",
    )
    .unwrap();
}

#[test]
fn insertion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {x => y}").unwrap();
    assert!(db
        .run_default(r"?[x, y] <- [[1, 2]] :insert a {x => y}",)
        .is_ok());
    assert!(db
        .run_default(r"?[x, y] <- [[1, 3]] :insert a {x => y}",)
        .is_err());
}

#[test]
fn deletion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {x => y}").unwrap();
    assert!(db.run_default(r"?[x] <- [[1]] :delete a {x}").is_err());
    assert!(db
        .run_default(r"?[x, y] <- [[1, 2]] :insert a {x => y}",)
        .is_ok());
    db.run_default(r"?[x] <- [[1]] :delete a {x}").unwrap();
}

#[test]
fn into_payload() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {x => y}").unwrap();
    db.run_default(r"?[x, y] <- [[1, 2], [3, 4]] :insert a {x => y}")
        .unwrap();

    let mut res = db.run_default(r"?[x, y] := *a[x, y]").unwrap();
    assert_eq!(res.rows.len(), 2);

    let delete = res.clone().into_payload("a", "rm");
    db.run_script(delete.0.as_str(), delete.1, ScriptMutability::Mutable)
        .unwrap();
    assert_eq!(
        db.run_default(r"?[x, y] := *a[x, y]").unwrap().rows.len(),
        0
    );

    db.run_default(r":create b {m => n}").unwrap();
    res.headers = vec!["m".into(), "n".into()];
    let put = res.into_payload("b", "put");
    db.run_script(put.0.as_str(), put.1, ScriptMutability::Mutable)
        .unwrap();
    assert_eq!(
        db.run_default(r"?[m, n] := *b[m, n]").unwrap().rows.len(),
        2
    );
}

#[test]
fn returning() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a {x => y}").unwrap();
    let res = db
        .run_default(r"?[x, y] <- [[1, 2]] :insert a {x => y} ")
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([["OK"]]));
    // for row in res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }

    let res = db
        .run_default(r"?[x, y] <- [[1, 3], [2, 4]] :returning :put a {x => y} ")
        .unwrap();
    assert_eq!(
        res.into_json()["rows"],
        json!([["inserted", 1, 3], ["inserted", 2, 4], ["replaced", 1, 2]])
    );
    // println!("{:?}", res.headers);
    // for row in res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }

    let res = db
        .run_default(r"?[x] <- [[1], [4]] :returning :rm a {x} ")
        .unwrap();
    // println!("{:?}", res.headers);
    // for row in res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }
    assert_eq!(
        res.into_json()["rows"],
        json!([
            ["requested", 1, null],
            ["requested", 4, null],
            ["deleted", 1, 3]
        ])
    );
    db.run_default(r":create todo{id:Uuid default rand_uuid_v1() => label: String, done: Bool}")
        .unwrap();
    let res = db
        .run_default(r"?[label,done] <- [['milk',false]] :put todo{label,done} :returning")
        .unwrap();
    assert_eq!(res.rows[0].len(), 4);
    for title in res.headers.iter() {
        print!("{} ", title);
    }
    println!();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
}

#[test]
fn parser_corner_case() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r#"?[x] := x = 1 or x = 2"#).unwrap();
    db.run_default(r#"?[C] := C = 1  orx[C] := C = 1"#).unwrap();
    db.run_default(r#"?[C] := C = true, C  inx[C] := C = 1"#)
        .unwrap();
    db.run_default(r#"?[k] := k in int_range(300)"#).unwrap();
    db.run_default(r#"ywcc[a] <- [[1]] noto[A] := ywcc[A] ?[A] := noto[A]"#)
        .unwrap();
}

#[test]
fn as_store_in_imperative_script() {
    let db = DbInstance::new("mem", "", "").unwrap();
    let res = db
        .run_default(
            r#"
    { ?[x, y, z] <- [[1, 2, 3], [4, 5, 6]] } as _store
    { ?[x, y, z] := *_store{x, y, z} }
    "#,
        )
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1, 2, 3], [4, 5, 6]]));
    let res = db
        .run_default(
            r#"
    {
        ?[y] <- [[1], [2], [3]]
        :create a {x default rand_uuid_v1() => y}
        :returning
    } as _last
    {
        ?[x] := *_last{_kind: 'inserted', x}
    }
    "#,
        )
        .unwrap();
    assert_eq!(3, res.rows.len());
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    assert!(db
        .run_default(
            r#"
    {
        ?[x, x] := x = 1
    } as _last
    "#
        )
        .is_err());

    let res = db
        .run_default(
            r#"
    {
        x[y] <- [[1], [2], [3]]
        ?[sum(y)] := x[y]
    } as _last
    {
        ?[sum_y] := *_last{sum_y}
    }
    "#,
        )
        .unwrap();
    assert_eq!(1, res.rows.len());
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
}

#[test]
fn update_shall_not_destroy_values() {
    let db = DbInstance::default();
    db.run_default(r"?[x, y] <- [[1, 2]] :create z {x => y default 0}")
        .unwrap();
    let r = db.run_default(r"?[x, y] := *z {x, y}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 2]]));
    db.run_default(r"?[x] <- [[1]] :update z {x}").unwrap();
    let r = db.run_default(r"?[x, y] := *z {x, y}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 2]]));
}

#[test]
fn update_shall_work() {
    let db = DbInstance::default();
    db.run_default(r"?[x, y, z] <- [[1, 2, 3]] :create z {x => y, z}")
        .unwrap();
    let r = db.run_default(r"?[x, y, z] := *z {x, y, z}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 2, 3]]));
    db.run_default(r"?[x, y] <- [[1, 4]] :update z {x, y}")
        .unwrap();
    let r = db.run_default(r"?[x, y, z] := *z {x, y, z}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 4, 3]]));
}

#[test]
fn sysop_in_imperatives() {
    let script = r#"
    {
            :create cm_src {
                aid: String =>
                title: String,
                author: String?,
                kind: String,
                url: String,
                domain: String?,
                pub_time: Float?,
                dt: Float default now(),
                weight: Float default 1,
            }
        }
        {
            :create cm_txt {
                tid: String =>
                aid: String,
                tag: String,
                follows_tid: String?,
                dup_for: String?,
                text: String,
                info_amount: Int,
            }
        }
        {
            :create cm_seg {
                sid: String =>
                tid: String,
                tag: String,
                part: Int,
                text: String,
                vec: <F32; 1536>,
            }
        }
        {
            ::hnsw create cm_seg:vec {
                dim: 1536,
                m: 50,
                dtype: F32,
                fields: vec,
                distance: Cosine,
                ef: 100,
            }
        }
        {
            ::lsh create cm_txt:lsh {
                extractor: text,
                extract_filter: is_null(dup_for),
                tokenizer: NGram,
                n_perm: 200,
                target_threshold: 0.5,
                n_gram: 7,
            }
        }
        {::relations}
    "#;
    let db = DbInstance::default();
    db.run_default(script).unwrap();
}

#[test]
fn bad_parse() {
    let db = DbInstance::default();
    db.run_default(
        r"
        :create named_hero_history {
        name: String,
        value: Bool,
        when: Int
    }",
    )
    .unwrap();
    db.run_default(r"
        last_named_hero[first, first, max(hist)] := *named_hero_history[first, first, value, hist], hist <= 1;

        some_named_hero[first, first, value] := last_named_hero[first, first, last], *named_hero_history[first, first, value, last];

        named_hero[first, first, value] := cast[first], value = false, not some_named_hero[first, first, _];
        named_hero[first, first, value] := some_named_hero[first, first, value];
        ?[hero] :=
    ").expect_err("should fail");
}

#[test]
fn puts() {
    let db = DbInstance::default();
    db.run_default(
        r"
            :create cm_txt {
                tid: String =>
                aid: String,
                tag: String,
                follows_tid: String? default null,
                for_qs: [String] default [],
                dup_for: String? default null,
                text: String,
                seg_vecs: [<F32; 1536>],
                seg_pos: [(Int, Int)],
                format: String default 'text',
                info_amount: Int,
            }
    ",
    )
    .unwrap();
    db.run_default(
        r"
        ?[tid, aid, tag, text, info_amount, dup_for, seg_vecs, seg_pos] := dup_for = null,
                tid = 'x', aid = 'y', tag = 'z', text = 'w', info_amount = 12,
                follows_tid = null, for_qs = [], format = 'x',
                seg_vecs = [], seg_pos = [[0, 10]]
        :put cm_txt {tid, aid, tag, text, info_amount, seg_vecs, seg_pos, dup_for}
    ",
    )
    .unwrap();
}

#[test]
fn short_hand() {
    let db = DbInstance::default();
    db.run_default(r":create x {x => y, z}").unwrap();
    db.run_default(r"?[x, y, z] <- [[1, 2, 3]] :put x {}")
        .unwrap();
    let r = db.run_default(r"?[x, y, z] := *x {x, y, z}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 2, 3]]));
}

#[test]
fn param_shorthand() {
    let db = DbInstance::default();
    db.run_script(
        r"
        ?[] <- [[$x, $y, $z]]
        :create x {}
    ",
        BTreeMap::from([
            ("x".to_string(), DataValue::from(1)),
            ("y".to_string(), DataValue::from(2)),
            ("z".to_string(), DataValue::from(3)),
        ]),
        ScriptMutability::Mutable,
    )
    .unwrap();
    let res = db.run_default(r"?[x, y, z] := *x {x, y, z}");
    assert_eq!(res.unwrap().into_json()["rows"], json!([[1, 2, 3]]));
}

#[test]
fn crashy_imperative() {
    let db = DbInstance::default();
    db.run_default(
        r"
        {:create _test {a}}

        %loop
            %if { len[count(x)] := *_test[x]; ?[x] := len[z], x = z >= 10 }
                %then %return _test
            %end
            { ?[a] := a = rand_uuid_v1(); :put _test {a} }
        %end
        ",
    )
    .unwrap();
}

#[test]
fn hnsw_index() {
    // NOTE (mnestic 0.12.2): the inherited schema below used
    // `last_accessed_at: Validity default [floor(now()), true]`. `floor(now())` is a float in
    // SECONDS, and a Validity timestamp is an integer in MICROSECONDS — so every row this test
    // wrote was stamped at 1970, ~1e6x too small. The test never asserted on the value, so it
    // never noticed. It was the ONLY caller of the validity float channel in the entire tree,
    // and it was in our own suite. See docs/plans/mnestic-0121-0130/design-0122.md.
    let db = DbInstance::default();
    db.run_default(
        r#"
        :create beliefs {
            belief_id: Uuid,
            character_id: Uuid,
            belief: String,
            last_accessed_at: Validity default [to_int(now() * 1000000), true],
            =>
            details: String default "",
            parent_belief_id: Uuid? default null,
            valence: Float default 0,
            aspects: [(String, Float, String, String)] default [],
            belief_embedding: <F32; 768>,
            details_embedding: <F32; 768>,
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::hnsw create beliefs:embedding_space {
            dim: 768,
            m: 50,
            dtype: F32,
            fields: [belief_embedding, details_embedding],
            distance: Cosine,
            ef_construction: 20,
            extend_candidates: false,
            keep_pruned_connections: false,
        }
    "#,
    )
    .unwrap();
    db.run_default(r#"
        ?[belief_id, character_id, belief, belief_embedding, details_embedding] <- [[rand_uuid_v1(), rand_uuid_v1(), "test", rand_vec(768), rand_vec(768)]]
        :put beliefs {}
    "#).unwrap();
    let res = db.run_default(r#"
            ?[belief, valence, dist, character_id, vector] := ~beliefs:embedding_space{ belief, valence, character_id |
                query: rand_vec(768),
                k: 100,
                ef: 20,
                radius: 1.0,
                bind_distance: dist,
                bind_vector: vector
            }

            :order -valence
            :order dist
    "#).unwrap();
    println!("{}", res.into_json()["rows"][0][4]);
}

#[test]
fn fts_drop() {
    let db = DbInstance::default();
    db.run_default(
        r#"
            :create entity {name}
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::fts create entity:fts_index { extractor: name,
            tokenizer: Simple, filters: [Lowercase]
        }
    "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::fts drop entity:fts_index
    "#,
    )
    .unwrap();
}

// ==== mnestic fork: temporal-axis rule at :create (bitemporality step 3) ====

#[test]
fn txtime_create_validation() {
    let db = DbInstance::new("mem", "", "").unwrap();
    let expect_axis_err = |script: &str, needle: &str| {
        let err = db.run_default(script).expect_err(script);
        // collapse miette's line-wrapping so needles match across breaks
        let msg = format!("{err:?}")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            msg.contains("invalid temporal-axis declaration"),
            "{script}: {msg}"
        );
        assert!(
            msg.contains(needle),
            "{script}: expected `{needle}` in: {msg}"
        );
        // the copy-pasteable corrected declaration is in the help text
        assert!(
            msg.contains(":create"),
            "{script}: no corrected form in: {msg}"
        );
    };

    expect_axis_err(
        ":create r_val {k => v: Int, tt: TxTime}",
        "key column, not a value column",
    );
    expect_axis_err(":create r_pos {tt: TxTime, k => v: Int}", "last key column");
    expect_axis_err(
        ":create r_ord {k, tt: TxTime, v: Validity => x: Int}",
        "last key column",
    );
    expect_axis_err(
        ":create r_two {k, t1: TxTime, t2: TxTime => v: Int}",
        "at most one TxTime",
    );
    expect_axis_err(
        ":create r_2vt {v1: Validity, v2: Validity, tt: TxTime}",
        "at most one Validity",
    );
    expect_axis_err(
        ":create r_null {k, tt: TxTime? => v: Int}",
        "cannot be nullable",
    );
    expect_axis_err(
        ":create r_gap {v: Validity, k, tt: TxTime}",
        "immediately precede",
    );

    // Valid shapes: tt-only (system-versioned) and bitemporal.
    db.run_default(":create audit {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default(":create belief {e, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    let cols = db.run_default("::columns audit").unwrap().into_json();
    let rendered = cols["rows"].to_string();
    assert!(rendered.contains("TxTime"), "{rendered}");
}

#[test]
fn txtime_create_rejected_on_temp_relations() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // inside a multi-statement script, `_`-relations are legitimate temps
    let err = db
        .run_default("{:create _tmp {k, tt: TxTime => v: Int}} {?[k] <- [[1]]}")
        .expect_err("temp TxTime must be rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("transaction-temp"), "{msg}");
}

#[test]
fn txtime_user_supplied_value_rejected() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit2 {k, tt: TxTime => v: Int}")
        .unwrap();
    let err = db
        .run_default("?[k, tt, v] <- [[1, 123, 2]] :put audit2 {k, tt => v}")
        .expect_err("user-supplied tt must be rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("engine-assigned"), "{msg}");
}

// ==== mnestic fork: tt write path (bitemporality step 3b) ====

#[test]
fn txtime_put_stamps_at_commit() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_w {k, tt: TxTime => v: Int}")
        .unwrap();

    // Deferred read-your-writes: the same script does NOT see its own write.
    let res = db
        .run_default("{?[k, v] <- [[1, 10]] :put audit_w {k => v}} {?[k] := *audit_w[k, tt, v]}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);

    // The next script does.
    let res = db
        .run_default("?[k, v] := *audit_w[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // Capture a tt point between the two versions, then correct.
    let DbInstance::Mem(inner) = &db else {
        panic!()
    };
    let between = inner.tt_clock().peek() + 1;
    db.run_default("?[k, v] <- [[1, 20]] :put audit_w {k => v}")
        .unwrap();

    // Bare read = CURRENT STATE (the correction only) — §4 migration invariant.
    let res = db
        .run_default("?[k, v] := *audit_w[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 20]]));

    // As-of the point between the versions: the original belief.
    let res = db
        .run_default(&format!("?[k, v] := *audit_w[k, tt, v @ (tt: {between})]"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // As-of before the first write: nothing was known.
    let res = db
        .run_default("?[k, v] := *audit_w[k, tt, v @ (tt: 1)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);

    // @ (tt: 'NOW') is the explicit spelling of the current-state default.
    let res = db
        .run_default("?[k, v] := *audit_w[k, tt, v @ (tt: 'NOW')]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 20]]));
}

#[test]
fn txtime_same_tx_double_put_is_last_write_wins() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_lww {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10], [1, 20]] :put audit_lww {k => v}")
        .unwrap();
    let res = db
        .run_default("?[v] := *audit_lww[k, tt, v]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "same (key, tt) collapses: {rows:?}");
}

#[test]
fn txtime_rm_appends_retraction() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_rm {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put audit_rm {k => v}")
        .unwrap();
    let DbInstance::Mem(inner) = &db else {
        panic!()
    };
    let before_rm = inner.tt_clock().peek() + 1;
    db.run_default("?[k] <- [[1]] :rm audit_rm {k}").unwrap();

    // Current state: the key is believed-deleted -> absent.
    let res = db
        .run_default("?[k] := *audit_rm[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);

    // As-of before the removal: still there — nothing was physically deleted.
    let res = db
        .run_default(&format!(
            "?[k, v] := *audit_rm[k, tt, v @ (tt: {before_rm})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // rm again: already believed-deleted, a no-op.
    db.run_default("?[k] <- [[1]] :rm audit_rm {k}").unwrap();

    // rm of a missing key: no-op; :delete of a missing key: error.
    db.run_default("?[k] <- [[999]] :rm audit_rm {k}").unwrap();
    let err = db
        .run_default("?[k] <- [[999]] :delete audit_rm {k}")
        .expect_err(":delete missing must fail");
    assert!(format!("{err:?}").contains("does not exist"), "{err:?}");
}

#[test]
fn txtime_bitemporal_puts_and_conflicts() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create belief_w {e, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    // assert + later cessation on the vt axis — two separate transactions
    db.run_default("?[e, v, x] <- [[1, 'ASSERT', 10]] :put belief_w {e, v => x}")
        .unwrap();
    db.run_default("?[e, v, x] <- [[1, 'RETRACT', 10]] :put belief_w {e, v => x}")
        .unwrap();
    let res = db
        .run_default("?[v, x] := *belief_w[e, v, tt, x]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 2);

    // assert AND retract of one (key, vt) in ONE tx: unbreakable tie -> error
    let err = db
        .run_default(
            "?[e, v, x] <- [[2, [123, true], 1], [2, [123, false], 1]] :put belief_w {e, v => x}",
        )
        .expect_err("assert+retract same (key, vt) in one tx must fail");
    assert!(
        format!("{err:?}").contains("asserts AND retracts"),
        "{err:?}"
    );

    // :rm on bitemporal (4c): a cessation at the supplied valid time —
    // 'RETRACT' coerces to (now, retract), i.e. "ceases now"
    db.run_default("?[e, v] <- [[1, 'RETRACT']] :rm belief_w {e, v}")
        .unwrap();
    let res = db
        .run_default("?[x] := *belief_w[e, v, tt, x @ 'NOW']")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0, "ceased now");
}

#[test]
fn txtime_unsupported_ops_error() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_ops {k, tt: TxTime => v: Int}")
        .unwrap();
    // 4c: these now work — only :replace stays rejected
    let err = db
        .run_default("?[k, v] <- [[1, 1]] :replace audit_ops {k => v}")
        .expect_err("replace must fail");
    assert!(format!("{err:?}").contains("history"), "{err:?}");

    let err = db
        .run_default(r#"::set_triggers audit_ops on put { ?[k] <- [[1]] }"#)
        .expect_err("triggers must fail");
    assert!(format!("{err:?}").contains("not supported"), "{err:?}");

    let err = db
        .run_default("::index create audit_ops:by_v {v}")
        .expect_err("index create must fail");
    assert!(format!("{err:?}").contains("not supported"), "{err:?}");
}

#[test]
fn txtime_rows_persist_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tt_rows.db");
    let path_str = path.to_str().unwrap().to_string();
    {
        let db = DbInstance::new("sqlite", &path_str, "").unwrap();
        db.run_default(":create audit_p {k, tt: TxTime => v: Int}")
            .unwrap();
        db.run_default("?[k, v] <- [[1, 10]] :put audit_p {k => v}")
            .unwrap();
    }
    let db = DbInstance::new("sqlite", &path_str, "").unwrap();
    let res = db
        .run_default("?[k, v] := *audit_p[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));
}

#[test]
fn txtime_import_relations_one_tt_per_batch() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_imp {k, tt: TxTime => v: Int}")
        .unwrap();

    // import two rows in one batch: both get the SAME tt (one belief event)
    let payload = serde_json::json!({
        "audit_imp": {"headers": ["k", "v"], "rows": [[1, 10], [2, 20]]}
    });
    db.import_relations_str_with_err(&payload.to_string())
        .unwrap();
    let res = db
        .run_default("?[k, tt, v] := *audit_imp[k, tt, v]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][1], rows[1][1], "one tt per import batch: {rows:?}");

    // importing a tt column is rejected
    let bad = serde_json::json!({
        "audit_imp": {"headers": ["k", "tt", "v"], "rows": [[3, 1, 30]]}
    });
    let err = db
        .import_relations_str_with_err(&bad.to_string())
        .expect_err("tt header must be rejected");
    assert!(format!("{err:?}").contains("engine-assigned"), "{err:?}");

    // delete-imports are rejected
    let del = serde_json::json!({
        "-audit_imp": {"headers": ["k"], "rows": [[1]]}
    });
    let err = db
        .import_relations_str_with_err(&del.to_string())
        .expect_err("delete-import must be rejected");
    assert!(format!("{err:?}").contains("use :rm"), "{err:?}");
}

#[test]
fn txtime_restore_backup_reseeds_clock() {
    // Build a source store whose clock is far in the future, back it up,
    // restore into a fresh store: the fresh clock must jump past the mark.
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("src.db");
    let backup_path = dir.path().join("backup.db");
    let dst_path = dir.path().join("dst.db");

    let far_future;
    {
        let db = DbInstance::new("sqlite", src_path.to_str().unwrap(), "").unwrap();
        db.run_default(":create audit_b {k, tt: TxTime => v: Int}")
            .unwrap();
        let DbInstance::Sqlite(inner) = &db else {
            panic!()
        };
        far_future = inner
            .tt_clock()
            .advance_with_now(crate::runtime::tt_clock::wall_clock_micros() + 3_600_000_000);
        db.run_default("?[k, v] <- [[1, 10]] :put audit_b {k => v}")
            .unwrap();
        db.backup_db(backup_path.to_str().unwrap()).unwrap();
    }

    let db = DbInstance::new("sqlite", dst_path.to_str().unwrap(), "").unwrap();
    db.restore_backup(backup_path.to_str().unwrap()).unwrap();
    let DbInstance::Sqlite(inner) = &db else {
        panic!()
    };
    assert!(
        inner.tt_clock().peek() > far_future,
        "restore must re-seed the clock past the restored mark"
    );
    // and the restored row is readable
    let res = db
        .run_default("?[k, v] := *audit_b[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // import_from_backup of a tt relation is rejected
    let err = db
        .import_from_backup(backup_path.to_str().unwrap(), &["audit_b".to_string()])
        .expect_err("import_from_backup of tt relation must be rejected");
    assert!(format!("{err:?}").contains("restore_backup"), "{err:?}");
}

#[test]
fn restore_backup_reseeds_relation_store_id() {
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("src-relid.db");
    let backup_path = dir.path().join("backup-relid.db");
    let dst_path = dir.path().join("dst-relid.db");

    {
        let src = DbInstance::new("sqlite", src_path.to_str().unwrap(), "").unwrap();
        src.run_default(":create alpha {k: Int => v: String}")
            .unwrap();
        src.run_default(":create beta {k: Int => v: String}")
            .unwrap();
        src.run_default("?[k, v] <- [[1, 'alpha-one']] :put alpha {k => v}")
            .unwrap();
        src.backup_db(backup_path.to_str().unwrap()).unwrap();
    }

    let dst = DbInstance::new("sqlite", dst_path.to_str().unwrap(), "").unwrap();
    dst.restore_backup(backup_path.to_str().unwrap()).unwrap();
    dst.run_default(":create gamma {k: Int => v: String}")
        .unwrap();
    dst.run_default("?[k, v] <- [[1, 'gamma-one']] :put gamma {k => v}")
        .unwrap();

    let alpha = dst
        .run_default("?[k, v] := *alpha{k, v}")
        .unwrap()
        .into_json();
    assert_eq!(alpha["rows"], json!([[1, "alpha-one"]]));
    let gamma = dst
        .run_default("?[k, v] := *gamma{k, v}")
        .unwrap()
        .into_json();
    assert_eq!(gamma["rows"], json!([[1, "gamma-one"]]));
}

#[test]
fn poisoned_relation_counter_is_repaired_on_open() {
    use crate::data::tuple::TupleT;
    use crate::runtime::relation::RelationId;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("poisoned-relid.db");
    {
        let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
        db.run_default(":create alpha {k: Int => v: String}")
            .unwrap();
        db.run_default(":create beta {k: Int => v: String}")
            .unwrap();
        db.run_default("?[k, v] <- [[1, 'beta-one']] :put beta {k => v}")
            .unwrap();

        let DbInstance::Sqlite(inner) = &db else {
            panic!()
        };
        let mut tx = inner.transact_write().unwrap();
        let counter_key = vec![DataValue::Null].encode_as_key(RelationId::SYSTEM);
        tx.store_tx
            .put(&counter_key, &RelationId::new(1).raw_encode())
            .unwrap();
        tx.commit_tx().unwrap();
    }

    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create gamma {k: Int => v: String}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 'gamma-one']] :put gamma {k => v}")
        .unwrap();
    let beta = db
        .run_default("?[k, v] := *beta{k, v}")
        .unwrap()
        .into_json();
    assert_eq!(beta["rows"], json!([[1, "beta-one"]]));
}

#[test]
fn corrupt_value_rows_error_and_can_be_repaired() {
    use crate::data::tuple::TupleT;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-value.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create damaged {k: Int => v: String}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 'intact']] :put damaged {k => v}")
        .unwrap();

    let DbInstance::Sqlite(inner) = &db else {
        panic!()
    };
    let mut tx = inner.transact_write().unwrap();
    let handle = tx.get_relation("damaged", false).unwrap();
    let key = vec![DataValue::from(1)].encode_as_key(handle.id);
    tx.store_tx.put(&key, &[0x91, 0x01, 0x02]).unwrap();
    tx.commit_tx().unwrap();
    drop(tx);

    let scan_error = db
        .run_default("?[k, v] := *damaged{k, v}")
        .expect_err("a corrupt row must fail a full scan without panicking");
    assert!(
        format!("{scan_error:?}").contains("eval::corrupt_value_blob"),
        "{scan_error:?}"
    );

    let lookup_error = db
        .run_default("wanted[k] <- [[1]] ?[v] := wanted[k], *damaged{k, v}")
        .expect_err("a fully-bound point lookup must not swallow decode errors");
    assert!(
        format!("{lookup_error:?}").contains("eval::corrupt_value_blob"),
        "{lookup_error:?}"
    );

    let repaired = db.run_default("::repair_corrupt damaged").unwrap();
    assert_eq!(repaired.rows, vec![vec![DataValue::from(1)]]);
    assert!(db
        .run_default("?[k, v] := *damaged{k, v}")
        .unwrap()
        .rows
        .is_empty());
}

#[test]
fn txtime_cross_statement_conflicts_rejected() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create belief_ms {e, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    // assert + retract of one (key, vt) across TWO statements of one script
    let err = db
        .run_default(
            "{?[e, v, x] <- [[9, [123, true], 1]] :put belief_ms {e, v => x}} \
             {?[e, v, x] <- [[9, [123, false], 1]] :put belief_ms {e, v => x}}",
        )
        .expect_err("cross-statement assert+retract must fail");
    assert!(
        format!("{err:?}").contains("asserts AND retracts"),
        "{err:?}"
    );

    // tt-only: :rm then :put of the same PRE-EXISTING key in one script
    db.run_default(":create a_ms {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put a_ms {k => v}")
        .unwrap();
    let err = db
        .run_default("{?[k] <- [[1]] :rm a_ms {k}} {?[k, v] <- [[1, 99]] :put a_ms {k => v}}")
        .expect_err("rm-then-put same key one tx must fail");
    assert!(
        format!("{err:?}").contains("asserts AND retracts"),
        "{err:?}"
    );

    // tt-only: :put then :rm of a key NOT in the store — clearer message
    let err = db
        .run_default("{?[k, v] <- [[5, 50]] :put a_ms {k => v}} {?[k] <- [[5]] :rm a_ms {k}}")
        .expect_err("put-then-rm same key one tx must fail");
    assert!(
        format!("{err:?}").contains("written in the same transaction"),
        "{err:?}"
    );
}

#[test]
fn txtime_delete_believed_deleted_errors() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a_bd {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put a_bd {k => v}")
        .unwrap();
    db.run_default("?[k] <- [[1]] :rm a_bd {k}").unwrap();
    let err = db
        .run_default("?[k] <- [[1]] :delete a_bd {k}")
        .expect_err(":delete of believed-deleted key must fail");
    assert!(format!("{err:?}").contains("believed-deleted"), "{err:?}");
}

#[test]
fn txtime_remove_relation_with_pending_writes_rejected() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create zomb {k, tt: TxTime => v: Int}")
        .unwrap();
    let err = db
        .run_default("{?[k, v] <- [[9, 9]] :put zomb {k => v}} {::remove zomb}")
        .expect_err("::remove with pending tt writes must fail");
    assert!(
        format!("{err:?}").contains("pending transaction-time writes"),
        "{err:?}"
    );
}

#[test]
fn txtime_create_with_rows_rejects_tt_header() {
    let db = DbInstance::new("mem", "", "").unwrap();
    let err = db
        .run_default("?[k, tt, v] <- [[1, 123, 2]] :create cw2 {k, tt: TxTime => v: Int}")
        .expect_err("tt header on :create-with-rows must fail");
    assert!(format!("{err:?}").contains("engine-assigned"), "{err:?}");
}

#[test]
fn txtime_axis_selector_errors() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a_asof {k, tt: TxTime => v: Int}")
        .unwrap();
    // bare @ E means valid time, everywhere: tt-only relations reject it
    let err = db
        .run_default("?[k, v] := *a_asof[k, tt, v @ 'NOW']")
        .expect_err("bare @ on tt-only must error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("no valid-time axis") || msg.contains("system-versioned"),
        "{msg}"
    );
    let err = db
        .run_default("?[k, v] := *a_asof[k, tt, v @ (vt: 'NOW')]")
        .expect_err("vt label on tt-only must error");
    assert!(format!("{err:?}").contains("valid-time"), "{err:?}");

    // tt label on a plain relation errors
    db.run_default(":create plain_r {k => v: Int}").unwrap();
    let err = db
        .run_default("?[k, v] := *plain_r[k, v @ (tt: 'NOW')]")
        .expect_err("tt label on plain must error");
    assert!(
        format!("{err:?}").contains("no transaction-time axis"),
        "{err:?}"
    );

    // tt label on a vt-only relation errors; vt label still works
    db.run_default(":create vt_r {k, v: Validity => x: Int}")
        .unwrap();
    let err = db
        .run_default("?[k, x] := *vt_r[k, v, x @ (tt: 'NOW')]")
        .expect_err("tt label on vt-only must error");
    assert!(
        format!("{err:?}").contains("no transaction-time axis"),
        "{err:?}"
    );
    db.run_default("?[k, x] := *vt_r[k, v, x @ (vt: 'NOW')]")
        .unwrap();

    // bitemporal: selectors resolve via the two-level scan (step 4b)
    db.run_default(":create bi_r {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, x] := *bi_r[k, v, tt, x @ (tt: 'NOW')]")
        .unwrap();
    db.run_default("?[k, x] := *bi_r[k, v, tt, x]").unwrap();

    // duplicate axis label is a parse error
    let err = db
        .run_default("?[k, v] := *a_asof[k, tt, v @ (tt: 1, tt: 2)]")
        .expect_err("duplicate label must fail");
    assert!(
        format!("{err:?}").contains("duplicate temporal axis"),
        "{err:?}"
    );

    // labeled vt form works on vt relations, order-free pair parses
    db.run_default("?[k, x] := *vt_r[k, v, x @ (vt: 'NOW')]")
        .unwrap();
}

#[test]
fn txtime_bitemporal_double_assert_lww() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create belief_lww {e, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default(
        "?[e, v, x] <- [[1, [50, true], 10], [1, [50, true], 20]] :put belief_lww {e, v => x}",
    )
    .unwrap();
    let res = db
        .run_default("?[x] := *belief_lww[e, v, tt, x]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 1);
}

#[test]
fn txtime_trigger_on_plain_relation_writes_into_tt_relation() {
    // The one currently-working trigger/tt interaction: a put-trigger on a
    // PLAIN relation whose body writes into a tt relation — rows buffer and
    // stamp at the outer commit.
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create plain_src {k => v: Int}").unwrap();
    db.run_default(":create audit_trail {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default(
        "::set_triggers plain_src on put { ?[k, v] := _new[k, v] :put audit_trail {k => v} }",
    )
    .unwrap();
    db.run_default("?[k, v] <- [[7, 70]] :put plain_src {k => v}")
        .unwrap();
    let res = db
        .run_default("?[k, v] := *audit_trail[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[7, 70]]));
}

#[test]
fn txtime_abort_drops_rows_and_hwm_atomically() {
    // The HWM+rows same-tx atomicity obligation from step 2: a transaction
    // whose later statement fails must leave neither rows nor an advanced
    // persisted mark.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tt_atomic.db");
    let path_str = path.to_str().unwrap().to_string();
    let db = DbInstance::new("sqlite", &path_str, "").unwrap();
    db.run_default(":create a_at {k, tt: TxTime => v: Int}")
        .unwrap();

    // script: a valid buffered put, then a failing statement -> whole tx aborts
    let err = db
        .run_default("{?[k, v] <- [[1, 10]] :put a_at {k => v}} {?[k] <- [[1]] :delete a_at {k}}");
    assert!(
        err.is_err(),
        "the :delete of a not-yet-committed key must fail the tx"
    );

    // no rows visible...
    let res = db
        .run_default("?[k] := *a_at[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);
    // ...and no persisted mark (nothing tt-stamped has ever committed here)
    let DbInstance::Sqlite(inner) = &db else {
        panic!()
    };
    let tx = inner.transact().unwrap();
    assert_eq!(tx.read_persisted_tt_hwm().unwrap(), None);
}

// ==== mnestic fork: custom aggregate registration (semirings R0b) ====

/// A user ⊕ keeping the numeric maximum — a legitimate absorptive meet.
struct TestMaxi;
impl crate::data::aggr::MeetAggrObj for TestMaxi {
    fn init_val(&self) -> DataValue {
        DataValue::Null
    }
    fn update(&self, left: &mut DataValue, right: &DataValue) -> miette::Result<bool> {
        if *left == DataValue::Null || right > left {
            *left = right.clone();
            return Ok(*left != DataValue::Null);
        }
        Ok(false)
    }
}

/// A non-absorptive ⊕ (numeric addition) — illegal as a meet.
struct TestAdder;
impl crate::data::aggr::MeetAggrObj for TestAdder {
    fn init_val(&self) -> DataValue {
        DataValue::from(0)
    }
    fn update(&self, left: &mut DataValue, right: &DataValue) -> miette::Result<bool> {
        let l = left.get_float().unwrap_or(0.);
        let r = right.get_float().unwrap_or(0.);
        *left = DataValue::from(l + r);
        Ok(true)
    }
}

#[test]
fn custom_aggr_meet_in_recursion_converges() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.register_custom_aggr("maxi".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    // longest-reachable-value: recursive SCC using the custom meet
    let res = db
        .run_default(
            r#"
        edges[f, t, w] <- [[1, 2, 10.0], [2, 3, 5.0], [1, 3, 2.0], [3, 4, 30.0]]
        reach[t, maxi(w)] := edges[1, t, w]
        reach[t, maxi(w2)] := reach[m, w], edges[m, t, w1], w2 = max(w, w1)
        ?[t, w] := reach[t, w]
        "#,
        )
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    // node 4 reachable with max edge weight 30 along the path
    assert!(rows.iter().any(|r| r[0] == 4 && r[1] == 30.0), "{rows:?}");
}

#[test]
fn custom_aggr_non_meet_rejected_in_recursion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.register_custom_aggr("summy".to_string(), false, || Box::new(TestAdder))
        .unwrap();
    // non-meet custom in a recursive SCC: stratifier must reject like a builtin
    let err = db
        .run_default(
            r#"
        edges[f, t] <- [[1, 2], [2, 3]]
        reach[t, summy(x)] := edges[1, t], x = 1
        reach[t, summy(x)] := reach[m, x], edges[m, t]
        ?[t, x] := reach[t, x]
        "#,
        )
        .expect_err("non-meet custom aggregate in recursion must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("stratif") || msg.contains("aggregation") || msg.contains("recursion"),
        "{msg}"
    );
}

#[test]
fn custom_aggr_non_recursive_uses_normal_adapter() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // is_meet=true in an all-meet head rides the meet path...
    db.register_custom_aggr("maxi2".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    let res = db
        .run_default("?[maxi2(x)] := x in [3, 9, 4]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[9]]));
    // ...while is_meet=false forces AggrKind::Normal — the MeetToNormalAdapter
    db.register_custom_aggr("maxinm".to_string(), false, || Box::new(TestMaxi))
        .unwrap();
    let res = db
        .run_default("?[maxinm(x)] := x in [3, 9, 4]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[9]]));
    // custom aggregates take no args in R0
    let err = db
        .run_default("?[maxinm(x, 5)] := x in [3, 9, 4]")
        .expect_err("args must be rejected");
    assert!(format!("{err:?}").contains("takes no arguments"), "{err:?}");
}

#[test]
fn custom_aggr_registry_policy() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // builtin names reserved
    let err = db
        .register_custom_aggr("min".to_string(), true, || Box::new(TestMaxi))
        .expect_err("builtin name must be reserved");
    assert!(format!("{err:?}").contains("reserved"), "{err:?}");
    // duplicates rejected; unregister-then-register works
    db.register_custom_aggr("dup".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    let err = db
        .register_custom_aggr("dup".to_string(), true, || Box::new(TestMaxi))
        .expect_err("duplicate must be rejected");
    assert!(format!("{err:?}").contains("already registered"), "{err:?}");
    assert!(db.unregister_custom_aggr("dup").unwrap());
    db.register_custom_aggr("dup".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    // unknown aggregate still errors cleanly
    let err = db
        .run_default("?[nosuch(x)] := x in [1]")
        .expect_err("unknown aggregate must fail");
    assert!(format!("{err:?}").contains("nosuch"), "{err:?}");
}

#[test]
fn custom_aggr_rejected_in_trigger_scripts() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.register_custom_aggr("maxi3".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    db.run_default(":create t_src {k => v: Int}").unwrap();
    db.run_default(":create t_dst {k => v}").unwrap();
    // trigger validation parses with an empty custom registry (R0 policy)
    let err = db
        .run_default(
            "::set_triggers t_src on put { ?[k, maxi3(v)] := _new[k, v] :put t_dst {k => v} }",
        )
        .expect_err("custom aggregate in trigger script must be rejected");
    assert!(format!("{err:?}").contains("maxi3"), "{err:?}");
}

#[test]
#[should_panic(expected = "non-idempotent meet aggregate")]
fn custom_aggr_debug_probe_catches_non_absorptive_meet() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // registered as meet, but ⊕ is addition: the debug probe must fire
    db.register_custom_aggr("badsum".to_string(), true, || Box::new(TestAdder))
        .unwrap();
    let _ = db.run_default(
        r#"
        edges[f, t] <- [[1, 2], [1, 3]]
        reach[f, badsum(x)] := edges[f, t], x = 1.0
        ?[f, x] := reach[f, x]
        "#,
    );
}

#[test]
fn meet_and_or_report_change_not_stability() {
    // mnestic fork fix: upstream returned the INVERTED changed-bit from the
    // and/or meet aggregates (true when stable), so a real change never
    // propagated through the semi-naive delta and stable values were kept in
    // it. Pin the corrected contract: true iff the value changed.
    use crate::data::aggr::{MeetAggrAnd, MeetAggrObj, MeetAggrOr};
    let and = MeetAggrAnd;
    let mut v = DataValue::from(true);
    assert!(and.update(&mut v, &DataValue::from(false)).unwrap());
    assert!(!and.update(&mut v, &DataValue::from(false)).unwrap());
    assert!(!and.update(&mut v, &DataValue::from(true)).unwrap());
    let or = MeetAggrOr;
    let mut v = DataValue::from(false);
    assert!(or.update(&mut v, &DataValue::from(true)).unwrap());
    assert!(!or.update(&mut v, &DataValue::from(true)).unwrap());
    assert!(!or.update(&mut v, &DataValue::from(false)).unwrap());
}

#[test]
fn txtime_negation_sees_current_state() {
    // Regression: negated atoms against tt-only relations hit NegJoin's
    // unreachable!() before StoredWithValidity gained a neg_join (the
    // current-state default made that the DEFAULT path for `not *audit{…}`).
    for engine in ["mem", "sqlite"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("neg.db");
        let db = DbInstance::new(engine, path.to_str().unwrap(), "").unwrap();
        db.run_default(":create audit_n {k, tt: TxTime => v: Int}")
            .unwrap();
        db.run_default("?[k, v] <- [[1, 10], [2, 20]] :put audit_n {k => v}")
            .unwrap();
        db.run_default("?[k] <- [[2]] :rm audit_n {k}").unwrap();

        // bare negation: key 1 exists (excluded), key 2 believed-deleted and
        // key 3 never existed (both included)
        let res = db
            .run_default("r[k] <- [[1], [2], [3]] ?[k] := r[k], not *audit_n{k}")
            .unwrap()
            .into_json();
        assert_eq!(res["rows"], serde_json::json!([[2], [3]]), "{engine}");

        // explicit selector on the negated atom
        let res = db
            .run_default("r[k] <- [[1], [2]] ?[k] := r[k], not *audit_n{k @ (tt: 'NOW')}")
            .unwrap()
            .into_json();
        assert_eq!(res["rows"], serde_json::json!([[2]]), "{engine}");

        // ::explain must not panic either (join_type)
        db.run_default("::explain { r[k] <- [[1]] ?[k] := r[k], not *audit_n{k} }")
            .unwrap();
    }
}

#[test]
fn vt_negation_with_selector_no_longer_panics() {
    // Pre-existing upstream panic, fixed by the same StoredWithValidity
    // neg_join: a negated vt atom with an @ selector.
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create vt_n {k, v: Validity => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, 'ASSERT', 5]] :put vt_n {k, v => x}")
        .unwrap();
    let res = db
        .run_default("r[k] <- [[1], [2]] ?[k] := r[k], not *vt_n{k @ 'NOW'}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[2]]));
}

#[test]
fn txtime_reads_on_sqlite_and_selector_forms() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forms.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create audit_f {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put audit_f {k => v}")
        .unwrap();
    let DbInstance::Sqlite(inner) = &db else {
        panic!()
    };
    let between = inner.tt_clock().peek() + 1;
    db.run_default("?[k, v] <- [[1, 20]] :put audit_f {k => v}")
        .unwrap();

    // named-field form with selector
    let res = db
        .run_default(&format!("?[k, w] := *audit_f{{k, v: w @ (tt: {between})}}"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // 'END' synonym = current state
    let res = db
        .run_default("?[k, w] := *audit_f{k, v: w @ (tt: 'END')}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 20]]));

    // date-only ISO string now parses (midnight UTC) — far past -> empty
    let res = db
        .run_default("?[k, w] := *audit_f{k, v: w @ (tt: '2001-01-01')}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);

    // order-free pair on a BITEMPORAL relation resolves (step 4b)
    db.run_default(":create bi_f {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, x] := *bi_f[k, v, tt, x @ (tt: 'NOW', vt: 'NOW')]")
        .unwrap();

    // nullable vt with TxTime is rejected at :create (the 8th shape)
    let err = db
        .run_default(":create bad_null_vt {k, v: Validity?, tt: TxTime => x: Int}")
        .expect_err("nullable vt with tt must be rejected");
    assert!(format!("{err:?}").contains("cannot be nullable"), "{err:?}");
}

// ==== mnestic fork: two-level bitemporal reads (step 4b) ====

/// A worked bitemporal history. Timeline (vt in abstract µs, tt per commit):
///   tt0: assert (vt=100, x=1)   — "1 from day 100"
///   tt1: assert (vt=200, x=2)   — "changed to 2 on day 200"
///   tt2: assert (vt=200, x=3)   — "correction: it was 3, not 2"
///   tt3: retract (vt=300)       — "ceased on day 300"
fn bitemporal_fixture(engine: &str, path: &str) -> (DbInstance, [i64; 4]) {
    let db = DbInstance::new(engine, path, "").unwrap();
    db.run_default(":create hist {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    fn peek(db: &DbInstance) -> i64 {
        match db {
            DbInstance::Mem(i) => i.tt_clock().peek(),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(i) => i.tt_clock().peek(),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(i) => i.tt_clock().peek(),
            _ => panic!("unsupported engine in fixture"),
        }
    }
    let mut tts = [0i64; 4];
    db.run_default("?[k, v, x] <- [[1, [100, true], 1]] :put hist {k, v => x}")
        .unwrap();
    tts[0] = peek(&db);
    db.run_default("?[k, v, x] <- [[1, [200, true], 2]] :put hist {k, v => x}")
        .unwrap();
    tts[1] = peek(&db);
    db.run_default("?[k, v, x] <- [[1, [200, true], 3]] :put hist {k, v => x}")
        .unwrap();
    tts[2] = peek(&db);
    db.run_default("?[k, v, x] <- [[1, [300, false], 0]] :put hist {k, v => x}")
        .unwrap();
    tts[3] = peek(&db);
    (db, tts)
}

#[test]
fn bitemporal_four_quadrants() {
    for engine in ["mem", "sqlite"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quad.db");
        let (db, tts) = bitemporal_fixture(engine, path.to_str().unwrap());
        let q = |vt: i64, tt: i64| -> serde_json::Value {
            db.run_default(&format!("?[x] := *hist[k, v, t, x @ (vt: {vt}, tt: {tt})]"))
                .unwrap()
                .into_json()["rows"]
                .clone()
        };
        assert_eq!(q(250, tts[3]), serde_json::json!([[3]]), "{engine}");
        assert_eq!(q(150, tts[3]), serde_json::json!([[1]]), "{engine}");
        assert_eq!(q(250, tts[1]), serde_json::json!([[2]]), "{engine}");
        assert_eq!(q(250, tts[0]), serde_json::json!([[1]]), "{engine}");
        assert_eq!(q(350, tts[3]).as_array().unwrap().len(), 0, "{engine}");
        assert_eq!(q(350, tts[2]), serde_json::json!([[3]]), "{engine}");
        assert_eq!(q(50, tts[3]).as_array().unwrap().len(), 0, "{engine}");
    }
}

#[test]
fn bitemporal_bare_scan_is_current_belief_per_group() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    let res = db
        .run_default("?[v, x] := *hist[k, v, t, x]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0][0][0], 300);
    assert_eq!(rows[0][0][1], serde_json::json!(false));
    assert_eq!(rows[1][0][0], 200);
    assert_eq!(rows[1][1], 3, "correction wins in the bare scan: {rows:?}");
    assert_eq!(rows[2][0][0], 100);
    assert_eq!(rows[2][1], 1);

    let res = db
        .run_default(&format!("?[v, x] := *hist[k, v, t, x @ (tt: {})]", tts[1]))
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][1], 2);

    let res = db
        .run_default("?[x] := *hist[k, v, t, x @ 250]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[3]]));
}

/// The step-6 pinned-iterator seek override on RocksDB must answer exactly
/// like the generic probe path: four quadrants (resolve-key mode) + the bare
/// scan (resolve-groups mode) + raw ::history.
#[cfg(feature = "storage-rocksdb")]
#[test]
fn bitemporal_reads_on_rocksdb() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quad_rocks");
    let (db, tts) = bitemporal_fixture("rocksdb", path.to_str().unwrap());
    let q = |vt: i64, tt: i64| -> serde_json::Value {
        db.run_default(&format!("?[x] := *hist[k, v, t, x @ (vt: {vt}, tt: {tt})]"))
            .unwrap()
            .into_json()["rows"]
            .clone()
    };
    assert_eq!(q(250, tts[3]), serde_json::json!([[3]]));
    assert_eq!(q(150, tts[3]), serde_json::json!([[1]]));
    assert_eq!(q(250, tts[1]), serde_json::json!([[2]]));
    assert_eq!(q(250, tts[0]), serde_json::json!([[1]]));
    assert_eq!(q(350, tts[3]).as_array().unwrap().len(), 0);
    assert_eq!(q(350, tts[2]), serde_json::json!([[3]]));
    assert_eq!(q(50, tts[3]).as_array().unwrap().len(), 0);

    let res = db
        .run_default("?[v, x] := *hist[k, v, t, x]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0][0][0], 300);
    assert_eq!(rows[0][0][1], serde_json::json!(false));
    assert_eq!(rows[1][1], 3, "correction wins in the bare scan");
    assert_eq!(rows[2][1], 1);

    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 4);
}

#[test]
fn bitemporal_cessation_across_runs_and_ties() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create hr {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 7]] :put hr {k, v => x}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, false], 0]] :put hr {k, v => x}")
        .unwrap();
    let res = db
        .run_default("?[x] := *hr[k, v, t, x @ (vt: 100)]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"].as_array().unwrap().len(),
        0,
        "the later cessation must win across the is_assert-run boundary"
    );

    let db2 = DbInstance::new("mem", "", "").unwrap();
    db2.run_default(":create hr2 {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db2.run_default("?[k, v, x] <- [[1, [100, false], 0]] :put hr2 {k, v => x}")
        .unwrap();
    db2.run_default("?[k, v, x] <- [[1, [100, true], 9]] :put hr2 {k, v => x}")
        .unwrap();
    let res = db2
        .run_default("?[x] := *hr2[k, v, t, x @ (vt: 100)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[9]]));
}

#[test]
fn bitemporal_repudiation_by_copy_and_chained_staleness() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create rep {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 100]] :put rep {k, v => x}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [300, true], 120]] :put rep {k, v => x}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [300, true], 100]] :put rep {k, v => x}")
        .unwrap();
    let res = db
        .run_default("?[x] := *rep[k, v, t, x @ (vt: 350)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[100]]));

    db.run_default("?[k, v, x] <- [[1, [100, true], 95]] :put rep {k, v => x}")
        .unwrap();
    let res = db
        .run_default("?[x] := *rep[k, v, t, x @ (vt: 350)]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[100]]),
        "copy is a snapshot"
    );
    let res = db
        .run_default("?[x] := *rep[k, v, t, x @ (vt: 150)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[95]]));
}

#[test]
fn bitemporal_negation_and_joins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("neg4b.db");
    let (db, _tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    let res = db
        .run_default("r[k] <- [[1], [2]] ?[k] := r[k], not *hist{k @ (vt: 250)}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[2]]));
    let res = db
        .run_default("r[k] <- [[1]] ?[k, x] := r[k], *hist{k, x @ (vt: 250)}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 3]]));
}

#[test]
fn as_of_pins_the_whole_query() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("asof.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    // also a tt-only relation in the same query
    db.run_default(":create audit_a {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put audit_a {k => v}")
        .unwrap();
    let after_audit = match &db {
        DbInstance::Sqlite(i) => i.tt_clock().peek(),
        _ => panic!(),
    };
    db.run_default("?[k, v] <- [[1, 20]] :put audit_a {k => v}")
        .unwrap();

    // :as_of pins BOTH tt-stamped atoms; the plain relation is untouched
    db.run_default(":create plain_a {k => v: Int}").unwrap();
    db.run_default("?[k, v] <- [[1, 5]] :put plain_a {k => v}")
        .unwrap();
    let res = db
        .run_default(&format!(
            "?[x, w, p] := *hist[k, v, t, x @ (vt: 250)], *audit_a[k2, t2, w], *plain_a[k3, p] \
             :as_of {}",
            tts[1].max(after_audit)
        ))
        .unwrap()
        .into_json();
    // hist's group-200 belief at that point (post-correction): 3;
    // audit_a as of then: 10; plain untouched: 5
    assert_eq!(res["rows"], serde_json::json!([[3, 10, 5]]));

    // explicit per-atom selector wins over :as_of
    let res = db
        .run_default(&format!(
            "?[w] := *audit_a[k, t, w @ (tt: 'NOW')] :as_of {after_audit}"
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[20]]));

    // :as_of with no tt-stamped relation in the query is an error
    let err = db
        .run_default("?[p] := *plain_a[k, p] :as_of 'NOW'")
        .expect_err(":as_of without tt relations must fail");
    let msg = format!("{err:?}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(msg.contains("no transaction-time-stamped"), "{msg}");
}

#[test]
fn as_of_minimal_probe() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_p2 {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put audit_p2 {k => v}")
        .unwrap();
    // no selector
    db.run_default("?[w] := *audit_p2[k, t, w] :as_of 'NOW'")
        .unwrap();
    // with explicit selector
    db.run_default("?[w] := *audit_p2[k, t, w @ (tt: 'NOW')] :as_of 'NOW'")
        .unwrap();
}

#[test]
fn temporal_join_columns_use_materialized_join() {
    // Regression: a join binding a temporal column used to clamp the prefix
    // scan to one vt-group — superseded/ceased values resurrected on sqlite,
    // BTreeMap::range panic on mem. The dispatch now falls back to a
    // materialized join over the RESOLVED scan.
    for engine in ["mem", "sqlite"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tj.db");
        let (db, _tts) = bitemporal_fixture(engine, path.to_str().unwrap());
        // join on (k, v): the belief at vt=120 (group 100) does not exist at
        // vt=250 (group 200 wins there) -> empty result
        let res = db
            .run_default(
                "l[k, v] := *hist[k, v, t, x @ (vt: 120)] \
                 ?[k, x2] := l[k, v], *hist[k, v, t2, x2 @ (vt: 250)]",
            )
            .unwrap()
            .into_json();
        assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{engine}");

        // negation twin: nothing at vt=250 carries group-100's vt value
        let res = db
            .run_default(
                "l[k, v] := *hist[k, v, t, x @ (vt: 120)] \
                 ?[k] := l[k, v], not *hist{k, v @ (vt: 250)}",
            )
            .unwrap()
            .into_json();
        assert_eq!(res["rows"], serde_json::json!([[1]]), "{engine}");

        // full-binding self-join with a value mismatch must be empty
        let res = db
            .run_default(
                "l[k, v, t, x] := *hist[k, v, t, x0 @ (vt: 250)], x = x0 - 1 \
                 ?[k, x] := l[k, v, t, x], *hist[k, v, t, x @ (vt: 250)]",
            )
            .unwrap()
            .into_json();
        assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{engine}");
    }
}

#[test]
fn vt_only_temporal_join_columns_fixed_too() {
    // The same defect class pre-existed upstream on the single-axis path.
    for engine in ["mem", "sqlite"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vtj.db");
        let db = DbInstance::new(engine, path.to_str().unwrap(), "").unwrap();
        db.run_default(":create vtx {k, v: Validity => x: Int}")
            .unwrap();
        db.run_default("?[k, v, x] <- [[1, [100, true], 7]] :put vtx {k, v => x}")
            .unwrap();
        db.run_default("?[k, v, x] <- [[1, [150, false], 0]] :put vtx {k, v => x}")
            .unwrap();
        // the belief at vt=120 (group 100) is retracted by vt=250: join empty
        let res = db
            .run_default("l[k, v] := *vtx[k, v, x @ 120] ?[k, x2] := l[k, v], *vtx[k, v, x2 @ 250]")
            .unwrap()
            .into_json();
        assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{engine}");
    }
}

#[test]
fn bitemporal_migration_invariant_comparative() {
    // §9: adding `tt: TxTime` to a vt relation changes no existing query's
    // results (up to corrections) — same puts into a vt twin, diff results.
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create m_vt {k, v: Validity => x: Int}")
        .unwrap();
    db.run_default(":create m_bt {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    for rel in ["m_vt", "m_bt"] {
        db.run_default(&format!(
            "?[k, v, x] <- [[1, [100, true], 1], [2, [100, true], 5]] :put {rel} {{k, v => x}}"
        ))
        .unwrap();
        db.run_default(&format!(
            "?[k, v, x] <- [[1, [200, true], 2]] :put {rel} {{k, v => x}}"
        ))
        .unwrap();
        db.run_default(&format!(
            "?[k, v, x] <- [[2, [300, false], 0]] :put {rel} {{k, v => x}}"
        ))
        .unwrap();
    }
    // bare scan
    let a = db
        .run_default("?[k, v, x] := *m_vt[k, v, x]")
        .unwrap()
        .into_json();
    let b = db
        .run_default("?[k, v, x] := *m_bt[k, v, t, x]")
        .unwrap()
        .into_json();
    assert_eq!(a["rows"], b["rows"], "bare scan must match the vt twin");
    // several @V points
    for vt in [50, 100, 150, 200, 250, 300, 350] {
        let a = db
            .run_default(&format!("?[k, x] := *m_vt[k, v, x @ {vt}]"))
            .unwrap()
            .into_json();
        let b = db
            .run_default(&format!("?[k, x] := *m_bt[k, v, t, x @ {vt}]"))
            .unwrap()
            .into_json();
        assert_eq!(a["rows"], b["rows"], "@{vt} must match the vt twin");
    }
}

#[test]
fn vt_equal_ts_assert_shadows_retract_pinned() {
    // §9: the previously-unpinned single-axis equal-ts behavior — an assert
    // and a retract at the SAME vt ts leave the assert visible (assert sorts
    // first; the skip-scan emits the first qualifying row).
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create eq_vt {k, v: Validity => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 7]] :put eq_vt {k, v => x}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, false], 0]] :put eq_vt {k, v => x}")
        .unwrap();
    let res = db
        .run_default("?[x] := *eq_vt[k, v, x @ 100]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[7]]));
}

// ==== mnestic fork: 4c existence-checking writes + bitemporal :rm ====

#[test]
fn tt_insert_update_ensure() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a4c {k, tt: TxTime => v: Int, w: Int default 0}")
        .unwrap();

    // insert new key OK
    db.run_default("?[k, v] <- [[1, 10]] :insert a4c {k => v}")
        .unwrap();
    // insert existing key fails
    let err = db
        .run_default("?[k, v] <- [[1, 11]] :insert a4c {k => v}")
        .expect_err("insert existing must fail");
    assert!(format!("{err:?}").contains("exists"), "{err:?}");
    // rm, then re-insert of the believed-deleted key succeeds
    db.run_default("?[k] <- [[1]] :rm a4c {k}").unwrap();
    db.run_default("?[k, v] <- [[1, 12]] :insert a4c {k => v}")
        .unwrap();
    let res = db
        .run_default("?[v] := *a4c[k, tt, v, w]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[12]]));
    // insert + put same key in one tx rejected
    let err = db
        .run_default(
            "{?[k, v] <- [[7, 1]] :put a4c {k => v}} {?[k, v] <- [[7, 2]] :insert a4c {k => v}}",
        )
        .expect_err("insert-after-put same tx must fail");
    assert!(format!("{err:?}").contains("already written"), "{err:?}");

    // update merges provided columns over the current belief
    db.run_default("?[k, w] <- [[1, 5]] :update a4c {k => w}")
        .unwrap();
    let res = db
        .run_default("?[v, w] := *a4c[k, tt, v, w]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[12, 5]]),
        "v kept, w updated"
    );
    // update of a missing key fails
    let err = db
        .run_default("?[k, w] <- [[99, 5]] :update a4c {k => w}")
        .expect_err("update missing must fail");
    assert!(format!("{err:?}").contains("does not exist"), "{err:?}");

    // ensure passes on matching current belief, fails on mismatch
    db.run_default("?[k, v] <- [[1, 12]] :ensure a4c {k => v}")
        .unwrap();
    let err = db
        .run_default("?[k, v] <- [[1, 999]] :ensure a4c {k => v}")
        .expect_err("ensure mismatch must fail");
    assert!(format!("{err:?}").contains("mismatch"), "{err:?}");
    // ensure_not passes on missing, fails on existing
    db.run_default("?[k, v] <- [[404, 0]] :ensure_not a4c {k => v}")
        .unwrap();
    let err = db
        .run_default("?[k, v] <- [[1, 0]] :ensure_not a4c {k => v}")
        .expect_err("ensure_not existing must fail");
    assert!(format!("{err:?}").contains("exists"), "{err:?}");
}

#[test]
fn bitemporal_rm_remap_cessation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rm4c.db");
    let (db, _tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    // cease the key at vt=400 via :rm — values come from the belief at 400
    db.run_default("?[k, v] <- [[1, [400, true]]] :rm hist {k, v}")
        .unwrap();
    // hmm: the fixture already retracted at vt=300, so belief at 400 is
    // deleted — the rm is a no-op; use vt=250 instead where belief = 3
    db.run_default("?[k, v] <- [[1, [250, true]]] :rm hist {k, v}")
        .unwrap();
    let res = db
        .run_default("?[x] := *hist[k, v, t, x @ (vt: 260)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0, "ceased at 250");
    // belief below 250 unaffected
    let res = db
        .run_default("?[x] := *hist[k, v, t, x @ (vt: 240)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[3]]));
    // :delete at a vt with no belief errors
    let err = db
        .run_default("?[k, v] <- [[1, [50, true]]] :delete hist {k, v}")
        .expect_err("delete with no belief must fail");
    assert!(format!("{err:?}").contains("no belief"), "{err:?}");

    // bitemporal insert/update on the (vt=NOW) current belief
    db.run_default(":create bi4c {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 5]] :put bi4c {k, v => x}")
        .unwrap();
    let err = db
        .run_default("?[k, v, x] <- [[1, [200, true], 6]] :insert bi4c {k, v => x}")
        .expect_err("bitemporal insert on a key with recorded beliefs must fail");
    assert!(format!("{err:?}").contains("recorded beliefs"), "{err:?}");
    db.run_default("?[k, x] <- [[1, 9]] :update bi4c {k => x}")
        .unwrap();
    let res = db
        .run_default("?[v, x] := *bi4c[k, v, t, x @ (vt: 150)]")
        .unwrap()
        .into_json();
    // the correction lands in group 100 (the current belief's own group)
    assert_eq!(res["rows"], serde_json::json!([[[100, true], 9]]));
}

#[test]
fn imperative_return_braced_clause_no_panic() {
    // mnestic fork fix: `%return { <query> }` panicked with unreachable!()
    // (upstream bug — the match arm expected query_script_inner but the
    // grammar delivers imperative_clause).
    let db = DbInstance::new("mem", "", "").unwrap();
    let res = db
        .run_default(
            r#"
        {:create _t_ret {a}}
        %return { ?[x] <- [[1]] }
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1]]));
}

#[test]
fn tt_4c_review_pins() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create pin4c {k, tt: TxTime => v: Int}")
        .unwrap();
    // duplicate key within ONE :insert statement is rejected (was silent LWW)
    let err = db
        .run_default("?[k, v] <- [[7, 1], [7, 2]] :insert pin4c {k => v}")
        .expect_err("duplicate in-statement insert must fail");
    assert!(format!("{err:?}").contains("duplicate key"), "{err:?}");

    // ensure with a bound tt column is rejected (was silently ignored)
    db.run_default("?[k, v] <- [[1, 10]] :put pin4c {k => v}")
        .unwrap();
    let err = db
        .run_default("?[k, tt, v] <- [[1, 5, 10]] :ensure pin4c {k, tt => v}")
        .expect_err("tt-bound ensure must fail");
    assert!(format!("{err:?}").contains("engine-assigned"), "{err:?}");

    // ensure of a key rewritten in the same tx is an ambiguous assertion
    let err = db
        .run_default(
            "{?[k, v] <- [[1, 99]] :put pin4c {k => v}} {?[k, v] <- [[1, 99]] :ensure pin4c {k => v}}",
        )
        .expect_err("ensure of same-tx rewrite must fail");
    assert!(format!("{err:?}").contains("ambiguous"), "{err:?}");

    // bitemporal: ensure with a bound vt column is rejected
    db.run_default(":create pin_bi {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 5]] :put pin_bi {k, v => x}")
        .unwrap();
    let err = db
        .run_default("?[k, v, x] <- [[1, [100, true], 5]] :ensure pin_bi {k, v => x}")
        .expect_err("vt-bound ensure must fail");
    assert!(format!("{err:?}").contains("CURRENT belief"), "{err:?}");

    // update after cessation fails; tt-past read shows pre-update value
    let db2 = DbInstance::new("mem", "", "").unwrap();
    db2.run_default(":create pin_c {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db2.run_default("?[k, v, x] <- [[1, [100, true], 5]] :put pin_c {k, v => x}")
        .unwrap();
    let DbInstance::Mem(inner) = &db2 else {
        panic!()
    };
    let before = inner.tt_clock().peek() + 1;
    db2.run_default("?[k, x] <- [[1, 9]] :update pin_c {k => x}")
        .unwrap();
    let res = db2
        .run_default(&format!(
            "?[x] := *pin_c[k, v, t, x @ (vt: 150, tt: {before})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[5]]),
        "tt-past shows pre-update"
    );
    db2.run_default("?[k, v] <- [[1, [200, true]]] :rm pin_c {k, v}")
        .unwrap();
    let err = db2
        .run_default("?[k, x] <- [[1, 11]] :update pin_c {k => x}")
        .expect_err("update after cessation must fail");
    assert!(format!("{err:?}").contains("does not exist"), "{err:?}");
}

// ==== mnestic fork: step 5 sys ops ====

#[test]
fn history_sysop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h5.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    let rows = res["rows"].as_array().unwrap();
    // 4 physical records: retract@300, correction+original@200, assert@100
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(
        res["headers"],
        serde_json::json!(["k", "vt_ts", "op", "tt", "x"])
    );
    assert_eq!(rows[0][1], 300);
    assert_eq!(rows[0][2], "retract");
    assert_eq!(rows[1][1], 200);
    assert_eq!(rows[1][2], "assert");
    assert!(rows[1][3].as_i64().unwrap() <= tts[2] && rows[1][3].as_i64().unwrap() > tts[1]);
    // limit/offset
    let res = db
        .run_default("::history hist [[1]] 2 1")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 2);
    // tt-only shape has no vt_ts column
    db.run_default(":create a5 {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[9, 1]] :put a5 {k => v}")
        .unwrap();
    db.run_default("?[k] <- [[9]] :rm a5 {k}").unwrap();
    let res = db.run_default("::history a5 [[9]]").unwrap().into_json();
    assert_eq!(res["headers"], serde_json::json!(["k", "op", "tt", "v"]));
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], "retract");
    // non-tt relation errors
    db.run_default(":create plain5 {k => v: Int}").unwrap();
    let err = db
        .run_default("::history plain5 [[1]]")
        .expect_err("must fail");
    assert!(
        format!("{err:?}").contains("requires a TxTime relation"),
        "{err:?}"
    );
}

#[test]
fn history_gc_sysop_and_floor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gc5.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    // cutoff between the correction (tts[2]) and the retraction (tts[3]):
    // group 200 keeps only the correction (its belief at cutoff); the
    // superseded original@200 is dropped; groups 100/300 untouched.
    let cutoff = tts[2] + 1;
    let res = db
        .run_default(&format!("::history_gc hist {cutoff}"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0][0], 1, "exactly the superseded row dropped");
    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 3);
    // as-of at/above the cutoff still answers correctly
    let res = db
        .run_default(&format!(
            "?[x] := *hist[k, v, t, x @ (vt: 250, tt: {cutoff})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[3]]));
    // below the floor errors
    let err = db
        .run_default(&format!(
            "?[x] := *hist[k, v, t, x @ (vt: 250, tt: {})]",
            tts[0]
        ))
        .expect_err("below-floor read must fail");
    assert!(format!("{err:?}").contains("gc floor"), "{err:?}");
    // read-only guard
    let err = db
        .run_script(
            &format!("::history_gc hist {cutoff}"),
            Default::default(),
            ScriptMutability::Immutable,
        )
        .expect_err("gc in read-only must fail");
    assert!(format!("{err:?}").contains("read-only"), "{err:?}");
}

#[test]
fn evict_sysop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ev5.db");
    let (db, _tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    let res = db.run_default("::evict hist [[1]]").unwrap().into_json();
    assert_eq!(res["rows"][0][2], 4, "all four records hard-deleted");
    // gone from history and reads
    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);
    // audit row exists with a hash marker (not the key), and an eviction tt
    let res = db
        .run_default("?[r, key, n] := *mnestic_evict_audit[r, key, tt, n]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "hist");
    assert_eq!(rows[0][2], 4);
    let marker = rows[0][1].as_str().unwrap();
    assert!(
        !marker.contains('1') || marker.len() == 36,
        "salted uuid marker: {marker}"
    );
    // unredacted opts out
    db.run_default("?[k, v, x] <- [[2, [100, true], 5]] :put hist {k, v => x}")
        .unwrap();
    db.run_default("::evict hist [[2]] unredacted").unwrap();
    let res = db
        .run_default("?[key] := *mnestic_evict_audit[r, key, tt, n], n == 1")
        .unwrap()
        .into_json();
    assert!(res["rows"][0][0].as_str().unwrap().contains('2'), "{res:?}");
    // read-only guard
    let err = db
        .run_script(
            "::evict hist [[2]]",
            Default::default(),
            ScriptMutability::Immutable,
        )
        .expect_err("evict in read-only must fail");
    assert!(format!("{err:?}").contains("read-only"), "{err:?}");
}

#[test]
fn step5_review_pins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pins5.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());

    // same-tx pending tt writes would be stamped AFTER the eviction's
    // deletes and resurrect the key: the whole script must fail
    let err = db
        .run_default(
            "{?[k, v, x] <- [[1, [500, true], 9]] :put hist {k, v => x}} {::evict hist [[1]]}",
        )
        .expect_err("evict with pending tt writes must fail");
    assert!(
        format!("{err:?}").contains("pending transaction-time writes"),
        "{err:?}"
    );
    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    assert_eq!(
        res["rows"].as_array().unwrap().len(),
        4,
        "nothing committed"
    );

    // a no-op gc deletes nothing, so it must not raise the (irreversible)
    // floor — past reads stay exact
    let res = db.run_default("::history_gc hist 1").unwrap().into_json();
    assert_eq!(res["rows"][0][0], 0);
    assert_eq!(res["rows"][0][1], serde_json::Value::Null);
    let res = db
        .run_default(&format!(
            "?[x] := *hist[k, v, t, x @ (vt: 250, tt: {})]",
            tts[0]
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1]]));

    // a future cutoff can only be a typo
    let err = db
        .run_default("::history_gc hist 99999999999999999")
        .expect_err("future cutoff must fail");
    assert!(format!("{err:?}").contains("future"), "{err:?}");

    // the report carries the EFFECTIVE floor: an older-cutoff re-run does
    // not lower (or echo below) it
    let cutoff = tts[2] + 1;
    let res = db
        .run_default(&format!("::history_gc hist {cutoff}"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0][0], 1);
    assert_eq!(res["rows"][0][1].as_i64().unwrap(), cutoff);
    let res = db.run_default("::history_gc hist 5").unwrap().into_json();
    assert_eq!(res["rows"][0][0], 0);
    assert_eq!(
        res["rows"][0][1].as_i64().unwrap(),
        cutoff,
        "floor not lowered"
    );

    // an imperative program containing only a destructive sysop must get a
    // write transaction (on RocksDB the read-tx bridge rejects writes)
    db.run_default(&format!("{{::history_gc hist {cutoff}}}"))
        .unwrap();

    // access levels guard the destructive ops and history reads
    db.run_default("::access_level read_only hist").unwrap();
    let err = db
        .run_default("::evict hist [[1]]")
        .expect_err("read_only evict");
    assert!(
        format!("{err:?}").contains("Insufficient access level"),
        "{err:?}"
    );
    let err = db
        .run_default(&format!("::history_gc hist {cutoff}"))
        .expect_err("read_only gc");
    assert!(
        format!("{err:?}").contains("Insufficient access level"),
        "{err:?}"
    );
    db.run_default("::access_level hidden hist").unwrap();
    let err = db
        .run_default("::history hist [[1]]")
        .expect_err("hidden history");
    assert!(
        format!("{err:?}").contains("Insufficient access level"),
        "{err:?}"
    );
    db.run_default("::access_level normal hist").unwrap();

    // duplicate keys in one ::evict: one audit row with the true count (a
    // repeat would overwrite it with rows_deleted = 0)
    let res = db
        .run_default("::evict hist [[1], [1]]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 1);
    assert_eq!(res["rows"][0][2], 3);
    let res = db
        .run_default("?[n] := *mnestic_evict_audit[r, key, tt, n]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[3]]));

    // keys coerce through the column types: a mistyped ::evict key errors
    // loudly instead of silently evicting nothing
    db.run_default(":create typed5 {k: Int, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[7, 1], [8, 2]] :put typed5 {k => v}")
        .unwrap();
    assert!(
        db.run_default("::evict typed5 [['7']]").is_err(),
        "mistyped key must be loud"
    );

    // ::history output is key-ascending; limit/offset are strict pos_ints
    // (`2 -1` must not silently parse as the single limit `2 - 1`)
    let res = db
        .run_default("::history typed5 [[8], [7]]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0][0], 7, "key-asc order");
    assert!(
        db.run_default("::history typed5 [[7]] 2 -1").is_err(),
        "negative offset must not parse"
    );

    // synthesized history headers must not shadow user columns
    db.run_default(":create clash5 {k, tt: TxTime => op: String}")
        .unwrap();
    let err = db
        .run_default("::history clash5 [[1]]")
        .expect_err("header collision");
    assert!(format!("{err:?}").contains("collides"), "{err:?}");

    // the audit relation name is reserved: a pre-existing relation with a
    // divergent schema would be corrupted by the raw audit puts
    let dir2 = tempfile::tempdir().unwrap();
    let path2 = dir2.path().join("pins5b.db");
    let (db2, _) = bitemporal_fixture("sqlite", path2.to_str().unwrap());
    db2.run_default(":create mnestic_evict_audit {a => b: Float}")
        .unwrap();
    let err = db2
        .run_default("::evict hist [[1]]")
        .expect_err("reserved name");
    assert!(format!("{err:?}").contains("reserved"), "{err:?}");
}

// ==== mnestic fork: provenance semirings R1 — bounded-meet (top-k proofs) ====

#[test]
fn bounded_meet_top_k_basics() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // non-recursive: the k lowest-cost packs per group, one row each,
    // cost-ordered; a group with fewer than k keeps them all
    let res = db
        .run_default(
            r#"
        data[g, pack] <- [[1, ['a', 3.0]], [1, ['b', 1.0]], [1, ['c', 2.0]],
                          [1, ['d', 4.0]], [2, ['e', 5.0]]]
        ?[g, best] := data[g, pack], best = pack
        "#,
        )
        .unwrap();
    assert_eq!(res.rows.len(), 5, "sanity: data visible");
    let res = db
        .run_default(
            r#"
        data[g, pack] <- [[1, ['a', 3.0]], [1, ['b', 1.0]], [1, ['c', 2.0]],
                          [1, ['d', 4.0]], [2, ['e', 5.0]]]
        ?[g, min_cost_k(pack, 3)] := data[g, pack]
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([
            [1, ["b", 1.0]],
            [1, ["c", 2.0]],
            [1, ["a", 3.0]],
            [2, ["e", 5.0]]
        ]),
        "{res:?}"
    );
    // no grouping columns: one global k-set
    let res = db
        .run_default(
            r#"
        data[g, pack] <- [[1, ['a', 3.0]], [1, ['b', 1.0]], [2, ['e', 5.0]]]
        ?[min_cost_k(pack, 2)] := data[g, pack]
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[["b", 1.0]], [["a", 3.0]]]),
        "{res:?}"
    );
}

#[test]
fn bounded_meet_k_shortest_paths() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // 1→2→3 (2.0) beats 1→3 (3.0); the 3→1 back-edge creates cycles whose
    // paths all cost ≥ 4 — the top-2 must converge despite them
    let res = db
        .run_default(
            r#"
        edge[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 3.0], [3, 1, 1.0]]
        sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
        sp[t, min_cost_k(pack, 2)] := sp[m, p], edge[m, t, w],
                                      pack = [concat(first(p), [t]), last(p) + w]
        ?[pack] := sp[3, pack]
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[[[1, 2, 3], 2.0]], [[[1, 3], 3.0]]]),
        "{res:?}"
    );
}

#[test]
fn bounded_meet_divergence_capped() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // a negative-cost cycle improves the k-set forever: the changed-bit
    // never settles, the epoch cap must convert that into a loud error
    let err = db
        .run_default(
            r#"
        edge[f, t, w] <- [[1, 2, -1.0], [2, 1, -1.0]]
        sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
        sp[t, min_cost_k(pack, 2)] := sp[m, p], edge[m, t, w],
                                      pack = [concat(first(p), [t]), last(p) + w]
        ?[pack] := sp[2, pack]
        "#,
        )
        .expect_err("negative cycle must hit the epoch cap");
    assert!(format!("{err:?}").contains("did not converge"), "{err:?}");
}

#[test]
fn bounded_meet_relay_recursion_unstratifiable() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // pins the in-SCC half of the divergence guard's structural
    // precondition: cyclic recursion into a bounded-meet rule through a
    // relay is rejected outright (the cross-SCC half — poisoned edges into
    // an aggregated rule forced across a stratum boundary — is what keeps
    // acyclic feeders out; together they leave a bounded rule's own delta
    // as its only in-stratum input, so its changed epochs form a contiguous
    // prefix). If either half is ever relaxed, a displacement cycle could
    // improve the k-set only every other epoch — the epoch cap counts TOTAL
    // changed epochs (not a streak resetting on quiet epochs) so it stays
    // sound in that world too
    let err = db
        .run_default(
            r#"
        edge[f, t, w] <- [[1, 2, -1.0], [2, 1, -1.0]]
        relay[m, p] := sp[m, p]
        sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
        sp[t, min_cost_k(pack, 2)] := relay[m, p], edge[m, t, w],
                                      pack = [concat(first(p), [t]), last(p) + w]
        ?[pack] := sp[2, pack]
        "#,
        )
        .expect_err("relay recursion through a bounded-meet head must not stratify");
    assert!(format!("{err:?}").contains("unstratifiable"), "{err:?}");
}

#[test]
fn meet_bit_and_or_report_changes_accurately() {
    use crate::data::aggr::{MeetAggrBitAnd, MeetAggrBitOr, MeetAggrObj};
    // mnestic fork fix: a non-changing AND/OR must report false, or stable
    // values re-enter the semi-naive delta every epoch (the bool variants
    // were fixed earlier; the byte variants had the same defect)
    let and = MeetAggrBitAnd;
    let mut v = DataValue::Bytes(vec![0xf0]);
    assert!(!and.update(&mut v, &DataValue::Bytes(vec![0xff])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0xf0]));
    assert!(and.update(&mut v, &DataValue::Bytes(vec![0x0f])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0x00]));

    let or = MeetAggrBitOr;
    let mut v = DataValue::Bytes(vec![0xff]);
    assert!(!or.update(&mut v, &DataValue::Bytes(vec![0x0f])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0xff]));
    let mut v = DataValue::Bytes(vec![0x0f]);
    assert!(or.update(&mut v, &DataValue::Bytes(vec![0xf0])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0xff]));

    // first contact with the empty-bytes init_val sentinel seeds the lazy
    // identity from the operand and MUST report changed — this is the
    // branch the eval-side empty-rule seeding relies on for a later real
    // row to enter the semi-naive delta
    let mut v = DataValue::Bytes(vec![]);
    assert!(and.update(&mut v, &DataValue::Bytes(vec![0xff])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0xff]));
    let mut v = DataValue::Bytes(vec![]);
    assert!(or.update(&mut v, &DataValue::Bytes(vec![0x0f])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0x0f]));
}

#[test]
fn bounded_meet_validation() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default("?[g, pack] <- [[1, ['a', 1.0]]] :create bm {g, pack}")
        .unwrap();
    // missing k
    let err = db
        .run_default("?[g, min_cost_k(pack)] := *bm[g, pack]")
        .expect_err("missing k");
    assert!(
        format!("{err:?}").contains("exactly one argument"),
        "{err:?}"
    );
    // non-positive k
    let err = db
        .run_default("?[g, min_cost_k(pack, 0)] := *bm[g, pack]")
        .expect_err("k = 0");
    assert!(format!("{err:?}").contains("positive integer"), "{err:?}");
    // mixed with another aggregate
    let err = db
        .run_default("?[min_cost_k(pack, 2), count(g)] := *bm[g, pack]")
        .expect_err("mixed head");
    assert!(
        format!("{err:?}").contains("bounded-meet aggregate"),
        "{err:?}"
    );
    // not in the last position
    let err = db
        .run_default("?[min_cost_k(pack, 2), g] := *bm[g, pack]")
        .expect_err("not last");
    assert!(
        format!("{err:?}").contains("bounded-meet aggregate"),
        "{err:?}"
    );
    // malformed pack
    let err = db
        .run_default("?[g, min_cost_k(g, 2)] := *bm[g, pack]")
        .expect_err("bad pack");
    assert!(
        format!("{err:?}").contains("cannot compute 'min_cost_k'"),
        "{err:?}"
    );
}

#[test]
fn bounded_meet_does_not_cap_costratified_recursion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // a CONVERGED (here: non-recursive) bounded rule sharing a stratum with
    // an unrelated recursion needing more epochs than the cap must not kill
    // it — the guard counts only epochs in which some k-set actually changed
    let res = db
        .run_default(
            r#"
        base[pack] <- [[['a', 1.0]], [['b', 2.0]]]
        best[min_cost_k(pack, 2)] := base[pack]
        w[a] := a = 0
        w[a] := w[b], a = b + 1, a < 4300
        ?[count(a)] := w[a], best[p]
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[4300 * 2]]), "{res:?}");
}

// ==== mnestic fork: provenance semirings R2 — annotations persist in rows ====

/// R2's acceptance criterion — "an annotated derivation is materialized and
/// queryable without recompute" — is met by the tags-as-columns architecture
/// with NO row-format change: annotation values are ordinary DataValues, so
/// `:put` of an annotated query output persists them in the existing
/// memcomparable row format. This test pins the four contracts (including
/// the composition with the bitemporal tt axis) across a real reopen.
#[test]
fn semiring_tags_persist_in_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r2.db");
    let p = path.to_str().unwrap();
    {
        let db = DbInstance::new("sqlite", p, "").unwrap();
        // (a) meet-annotated derivation → stored relation
        db.run_default(":create sp_out {dst: Int => pack}").unwrap();
        let sp_script = |put: &str| {
            format!(
                r#"
            edge[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 3.0]]
            sp[t, min_cost(pack)] := t = 1, pack = [[1], 0.0]
            sp[t, min_cost(pack)] := sp[m, p], edge[m, t, w],
                                     pack = [concat(first(p), [t]), last(p) + w]
            ?[dst, pack] := sp[dst, pack]
            {put}
            "#
            )
        };
        db.run_default(&sp_script(":put sp_out {dst => pack}"))
            .unwrap();
        // (b) bounded-meet k rows per group → stored relation (pack in key)
        db.run_default(":create topk_out {dst: Int, pack => }")
            .unwrap();
        db.run_default(
            r#"
            edge[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 3.0]]
            sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
            sp[t, min_cost_k(pack, 2)] := sp[m, p], edge[m, t, w],
                                          pack = [concat(first(p), [t]), last(p) + w]
            ?[dst, pack] := sp[dst, pack]
            :put topk_out {dst, pack}
            "#,
        )
        .unwrap();
        // (c) annotated + tt: materialized beliefs carry engine-stamped
        // transaction time — annotated belief HISTORY
        db.run_default(":create belief {dst: Int, tt: TxTime => pack}")
            .unwrap();
        db.run_default(&sp_script(":put belief {dst => pack}"))
            .unwrap();
        // a later, cheaper route to 3 changes the belief
        db.run_default(
            r#"
            edge[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 0.5]]
            sp[t, min_cost(pack)] := t = 1, pack = [[1], 0.0]
            sp[t, min_cost(pack)] := sp[m, p], edge[m, t, w],
                                     pack = [concat(first(p), [t]), last(p) + w]
            ?[dst, pack] := sp[dst, pack]
            :put belief {dst => pack}
            "#,
        )
        .unwrap();
        // (d) custom-aggregate annotations materialize; the operator itself
        // is registration-scoped
        db.register_custom_aggr("fuse2".to_string(), true, || Box::new(TestMaxi))
            .unwrap();
        db.run_default(":create fused {k: Int => v}").unwrap();
        db.run_default(
            r#"
            data[k, v] <- [[1, 0.9], [1, 0.5]]
            agg[k, fuse2(v)] := data[k, v]
            ?[k, v] := agg[k, v]
            :put fused {k => v}
            "#,
        )
        .unwrap();
    }
    // reopen: no re-registration, everything readable without recompute
    let db = DbInstance::new("sqlite", p, "").unwrap();
    let res = db
        .run_default("?[dst, pack] := *sp_out[dst, pack]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[1, [[1], 0.0]], [2, [[1, 2], 1.0]], [3, [[1, 2, 3], 2.0]]]),
        "{res:?}"
    );
    let res = db
        .run_default("?[dst, pack] := *topk_out[dst, pack]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 4, "{res:?}");
    // current belief = the cheaper corrected route
    let res = db
        .run_default("?[pack] := *belief[3, t, pack]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[[[1, 3], 0.5]]]), "{res:?}");
    // annotated belief HISTORY: both materializations recorded with their tts
    let res = db
        .run_default("::history belief [[3]]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    // as-of the FIRST materialization's tt, the old belief answers
    let first_tt = rows[1][2].as_i64().unwrap();
    let res = db
        .run_default(&format!(
            "?[pack] := *belief[3, t, pack @ (tt: {first_tt})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[[[1, 2, 3], 2.0]]]),
        "{res:?}"
    );
    // (d) materialized custom-aggregate output readable with NO registry…
    let res = db
        .run_default("?[k, v] := *fused[k, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 0.9]]), "{res:?}");
    // …while re-computing loudly requires the registration
    let err = db
        .run_default("data[k, v] <- [[1, 0.9]] ?[k, fuse2(v)] := data[k, v]")
        .expect_err("unregistered aggregate must not resolve");
    assert!(format!("{err:?}").contains("not found"), "{err:?}");
}

// ==== mnestic fork: provenance semirings R3 — :reconcile (belief revision) ====

#[test]
fn reconcile_tt_only_belief_revision() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create facts {k: Int, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10], [2, 20], [3, 30]] :reconcile facts {k => v}")
        .unwrap();
    let res = db
        .run_default("?[k, v] := *facts[k, t, v]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[1, 10], [2, 20], [3, 30]]),
        "{res:?}"
    );
    // revision: 1 unchanged, 2 changed, 3 gone, 4 new
    db.run_default("?[k, v] <- [[1, 10], [2, 25], [4, 40]] :reconcile facts {k => v}")
        .unwrap();
    let res = db
        .run_default("?[k, v] := *facts[k, t, v]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[1, 10], [2, 25], [4, 40]]),
        "{res:?}"
    );
    // unchanged key 1: exactly ONE record (no history bloat)
    let res = db.run_default("::history facts [[1]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 1, "{res:?}");
    // retracted key 3: assert + retract, and the as-of read still answers
    let res = db.run_default("::history facts [[3]]").unwrap().into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][1], "retract");
    let first_tt = rows[1][2].as_i64().unwrap();
    let res = db
        .run_default(&format!("?[v] := *facts[3, t, v @ (tt: {first_tt})]"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[30]]), "{res:?}");
    // idempotence: an identical reconcile buffers nothing
    db.run_default("?[k, v] <- [[1, 10], [2, 25], [4, 40]] :reconcile facts {k => v}")
        .unwrap();
    let res = db.run_default("::history facts [[2]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 2, "{res:?}");
}

#[test]
fn reconcile_bitemporal_cessations() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create bt {k: Int, vld: Validity, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default(
        "?[k, vld, v] <- [[1, [100, true], 7], [1, [200, true], 8]] :reconcile bt {k, vld => v}",
    )
    .unwrap();
    // revision: group 100 corrected, group 200 dropped (cessation)
    db.run_default("?[k, vld, v] <- [[1, [100, true], 9]] :reconcile bt {k, vld => v}")
        .unwrap();
    let res = db
        .run_default("?[x] := *bt[k, v, t, x @ (vt: 150)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[9]]), "corrected: {res:?}");
    let res = db
        .run_default("?[x] := *bt[k, v, t, x @ (vt: 250)]")
        .unwrap()
        .into_json();
    // group 200 ceased: from vt 200 onward the fact is believed-deleted
    // (spec §3 — a deciding group does NOT fall through to older groups)
    assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{res:?}");
    // the cessation is recorded, not erased: as-of the FIRST event's tt the
    // old belief at vt 250 was 8
    let res = db.run_default("::history bt [[1]]").unwrap().into_json();
    let rows = res["rows"].as_array().unwrap();
    let first_tt = rows.iter().map(|r| r[3].as_i64().unwrap()).min().unwrap();
    let res = db
        .run_default(&format!(
            "?[x] := *bt[k, v, t, x @ (vt: 250, tt: {first_tt})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[8]]), "{res:?}");
}

/// The R3 acceptance scenario: retract a base fact, re-derive, reconcile —
/// derived annotations stay consistent, and "what did we believe, and why,
/// as of T" answers across the revision.
#[test]
fn reconcile_tms_retraction_end_to_end() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create base {f: Int, t: Int, tt: TxTime => w: Float}")
        .unwrap();
    db.run_default("?[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 3.0]] :put base {f, t => w}")
        .unwrap();
    // derived, annotated: top-2 cheapest paths per destination (proofs in key)
    db.run_default(":create paths {dst: Int, pack, tt: TxTime => }")
        .unwrap();
    let derive = r#"
        edge[f, t, w] := *base[f, t, ttx, w]
        sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
        sp[t, min_cost_k(pack, 2)] := sp[m, p], edge[m, t, w],
                                      pack = [concat(first(p), [t]), last(p) + w]
        ?[dst, pack] := sp[dst, pack]
        :reconcile paths {dst, pack}
    "#;
    db.run_default(derive).unwrap();
    let res = db
        .run_default("?[pack] := *paths[3, pack, ttx]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[[[1, 2, 3], 2.0]], [[[1, 3], 3.0]]]),
        "{res:?}"
    );
    // retract a base fact (the cheap 2→3 edge), re-derive, reconcile
    db.run_default("?[f, t] <- [[2, 3]] :rm base {f, t}")
        .unwrap();
    db.run_default(derive).unwrap();
    // derived annotations consistent with the post-retraction base: the
    // proof through the retracted edge is GONE, not orphaned
    let res = db
        .run_default("?[pack] := *paths[3, pack, ttx]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[[[1, 3], 3.0]]]), "{res:?}");
    // …and the old belief plus its justification still answers as-of T
    let res = db
        .run_default("::history paths [[3, [[1, 2, 3], 2.0]]]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "assert then retract: {rows:?}");
    let first_tt = rows[1][3].as_i64().unwrap();
    let res = db
        .run_default(&format!(
            "?[pack] := *paths[3, pack, ttx @ (tt: {first_tt})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[[[1, 2, 3], 2.0]], [[[1, 3], 3.0]]]),
        "the pre-retraction annotated belief, with proofs: {res:?}"
    );
}

#[test]
fn reconcile_validation() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create plain_r {k: Int => v: Int}")
        .unwrap();
    let err = db
        .run_default("?[k, v] <- [[1, 1]] :reconcile plain_r {k => v}")
        .expect_err("plain relation");
    assert!(
        format!("{err:?}").contains("requires a TxTime relation"),
        "{err:?}"
    );
    db.run_default(":create rc {k: Int, tt: TxTime => v: Int}")
        .unwrap();
    // conflicting duplicate keys in one output
    let err = db
        .run_default("?[k, v] <- [[1, 1], [1, 2]] :reconcile rc {k => v}")
        .expect_err("conflicting rows");
    assert!(format!("{err:?}").contains("conflicting rows"), "{err:?}");
    // an earlier pending write in the same transaction
    let err = db
        .run_default(
            "{?[k, v] <- [[9, 9]] :put rc {k => v}} {?[k, v] <- [[1, 1]] :reconcile rc {k => v}}",
        )
        .expect_err("pending write");
    assert!(format!("{err:?}").contains("only"), "{err:?}");
    // bitemporal rows must declare beliefs (assert flag)
    db.run_default(":create rcb {k: Int, vld: Validity, tt: TxTime => v: Int}")
        .unwrap();
    let err = db
        .run_default("?[k, vld, v] <- [[1, [100, false], 1]] :reconcile rcb {k, vld => v}")
        .expect_err("retract-flag row");
    assert!(format!("{err:?}").contains("declare beliefs"), "{err:?}");
    // empty output retracts every current belief
    db.run_default("?[k, v] <- [[1, 1], [2, 2]] :reconcile rc {k => v}")
        .unwrap();
    db.run_default("?[k, v] <- [] :reconcile rc {k => v}")
        .unwrap();
    let res = db
        .run_default("?[k, v] := *rc[k, t, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{res:?}");

    // the declaration is complete: NO other write to the relation in the
    // same transaction, before or after — including writes an IDEMPOTENT
    // reconcile leaves no pending trace of (review must-fix)
    db.run_default("?[k, v] <- [[1, 10]] :reconcile rc {k => v}")
        .unwrap();
    for later in [
        "{?[k, v] <- [[1, 10]] :reconcile rc {k => v}} {?[k, v] <- [[3, 30]] :put rc {k => v}}",
        "{?[k, v] <- [[1, 10]] :reconcile rc {k => v}} {?[k] <- [[1]] :rm rc {k}}",
        // idempotent first reconcile buffers nothing; the second must still bail
        "{?[k, v] <- [[1, 10]] :reconcile rc {k => v}} {?[k, v] <- [[2, 20]] :reconcile rc {k => v}}",
    ] {
        let err = db.run_default(later).expect_err("write after reconcile");
        assert!(format!("{err:?}").contains("reconcile"), "{later}: {err:?}");
    }
    // §5: the revision is invisible to later reads in the same script
    let res = db
        .run_default("{?[k, v] <- [[1, 99]] :reconcile rc {k => v}} {?[k, v] := *rc[k, t, v]}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]), "{res:?}");
    let res = db
        .run_default("?[k, v] := *rc[k, t, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 99]]), "{res:?}");
}
