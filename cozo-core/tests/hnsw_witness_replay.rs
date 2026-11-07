//! Optional diagnostic replay of a retained, rolled-back SQLite index.
//! Input paths are explicit; the source database is copied before mutation.
use cozo::{DbInstance, ScriptMutability};
use std::collections::BTreeMap;

#[test]
#[ignore = "requires MNESTIC_WITNESS_REPLAY_DIR with store.db and mutations.json"]
fn replay_full_scale_witness_mutations() {
    let source = std::path::PathBuf::from(std::env::var("MNESTIC_WITNESS_REPLAY_DIR").unwrap());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("replay.db");
    std::fs::copy(source.join("store.db"), &path).unwrap();
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "{}").unwrap();
    let inputs: Vec<BTreeMap<String, serde_json::Value>> =
        serde_json::from_slice(&std::fs::read(source.join("mutations.json")).unwrap()).unwrap();
    assert_eq!(inputs.len(), 200, "the frozen small workload has 200 writes");
    for (i, input) in inputs.into_iter().enumerate() {
        let params = input
            .into_iter()
            .map(|(k, v)| (k, cozo::DataValue::from(v)))
            .collect();
        db.run_script(
            "{ ?[cid,text,emb] <- [[$id,$text,$v]] :put chunk {cid=>text,emb} } \
            { ?[cid,eid] <- $mentions :put mention {cid,eid} } \
            { ?[src,dst] <- $links :put link {src,dst} }",
            params,
            ScriptMutability::Mutable,
        )
        .unwrap_or_else(|e| panic!("write {i}: {e:?}"));
        eprintln!("write {i} committed");
    }
}
