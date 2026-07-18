use cozo::{
    data::functions::current_validity, parse::parse_script, CustomAggrRegistries, DbInstance,
    ScriptMutability,
};

fn main() {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();
    let script = "?[a] := a in [1, 2, 3]";
    let cur_vld = current_validity();
    let empty_meet = Default::default();
    let empty_bounded = Default::default();
    let script_ast = parse_script(
        script,
        &Default::default(),
        &db.get_fixed_rules(),
        CustomAggrRegistries {
            meet: &empty_meet,
            bounded: &empty_bounded,
        },
        cur_vld,
    )
    .unwrap();
    println!("AST: {:?}", script_ast);
    let result = db
        .run_script_ast(script_ast, cur_vld, ScriptMutability::Immutable)
        .unwrap();
    println!("Result: {:?}", result);
}
