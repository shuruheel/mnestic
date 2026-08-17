use cozo::DbInstance;

#[test]
fn data_readers_require_explicit_opt_in() {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();
    let rules = db.get_fixed_rules();

    assert_eq!(
        rules.contains_key("CsvReader"),
        cfg!(feature = "data-import")
    );
    assert_eq!(
        rules.contains_key("JsonReader"),
        cfg!(feature = "data-import")
    );
    // The RDF reader rides its own stricter gate (`rdf-io`, which implies
    // `data-import` — see docs/specs/rdf-boundary-io.md §4).
    assert_eq!(rules.contains_key("RdfReader"), cfg!(feature = "rdf-io"));
}
