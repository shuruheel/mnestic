use cozo::{DbInstance, ScriptMutability};
fn main() {
    let path = std::env::args().nth(1).expect("fresh database path");
    let db = DbInstance::new("sqlite", &path, "").unwrap();
    let run = |s: &str| db.run_script(s, Default::default(), ScriptMutability::Mutable).unwrap();
    run(":create p {id:Int=>v:<F32;2>}");
    run("::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}");
    for i in 0..3 { run(&format!("?[id,v]<-[[{i},[{i},{i}]]] :put p{{id=>v}}")); }
    run("?[id]<-[[1]] :rm p{id}");
    let result = run("?[id] := ~p:g{id | query:vec([0,0]),k:30,ef:50} :order id");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[1][0].get_int(), Some(2));
    println!("PASS: extracted crate preserves the bridge survivor on SQLite");
}
