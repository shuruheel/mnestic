//! Consumer-shaped duplicate-vector regression. The key sets came from the two
//! failing aa7d8df7 diagnostic cases; HNSW level construction remains unseeded.
use std::collections::BTreeMap;

use cozo::{DataValue, DbInstance, ScriptMutability};

#[test]
fn duplicate_vectors_accept_consumer_sized_batches() {
    let cases: Vec<Vec<String>> =
        serde_json::from_str(include_str!("fixtures/hnsw_duplicate_keys.json")).unwrap();
    assert_eq!(cases.len(), 2);
    for keys in cases {
        assert_eq!(keys.len(), 201);
        let db = DbInstance::new("mem", "", "").unwrap();
        let run = |script: &str, params| {
            db.run_script(script, params, ScriptMutability::Mutable)
                .unwrap()
        };
        run(":create p {id:String=>v:<F32;4>}", BTreeMap::new());
        run("::hnsw create p:g {fields:[v],dim:4,dtype:F32,distance:Cosine,m:16,ef_construction:50,extend_candidates:false,keep_pruned_connections:false}", BTreeMap::new());
        for batch in keys.chunks(100) {
            let rows = batch
                .iter()
                .map(|key| {
                    DataValue::List(vec![
                        DataValue::from(key.as_str()),
                        DataValue::List(vec![
                            DataValue::from(1.),
                            DataValue::from(0.),
                            DataValue::from(0.),
                            DataValue::from(0.),
                        ]),
                    ])
                })
                .collect();
            run(
                "?[id,v] <- $rows :put p {id=>v}",
                BTreeMap::from([("rows".into(), DataValue::List(rows))]),
            );
        }
        assert_eq!(
            run("?[count(id)] := *p{id}", BTreeMap::new()).rows[0][0],
            DataValue::from(201)
        );
        // With identical distances the order is unspecified. Check that every
        // inserted row stays reachable when the full index is requested.
        let found = run(
            "?[id] := ~p:g{id | query:vec([1,0,0,0]),k:201,ef:400}",
            BTreeMap::new(),
        );
        let actual: std::collections::BTreeSet<_> = found
            .rows
            .into_iter()
            .map(|row| row[0].get_str().unwrap().to_owned())
            .collect();
        let expected = keys.into_iter().collect();
        assert_eq!(actual, expected);
    }
}
