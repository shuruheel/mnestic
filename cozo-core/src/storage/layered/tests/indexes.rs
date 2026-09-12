/*
 * Index relations through a branch cycle.
 *
 * Reads compose through a stack: an index stores its structure as ordinary rows, so a layer's
 * rewritten neighbour lists shadow the base's and traversal sees a coherent graph. Flatten does
 * not compose, and copying those rows would splice two independently evolved graphs into
 * something structurally invalid whose only symptom is silent recall loss. So flatten skips
 * them and the consumer drops and rebuilds.
 */

use std::collections::BTreeSet;

use miette::Result;

use super::{base, Fixture};
use crate::runtime::db::ScriptMutability;
use crate::storage::layered::{LayerRef, Stack};

const SCHEMA: &str = ":create vrec {id: String, at: Validity => v: <F32; 2>}";
const INDEX: &str = "::hnsw create vrec:idx {
    dim: 2, m: 50, dtype: F32, fields: [v], distance: L2, ef_construction: 20
}";

fn put(f: &Fixture, stack: &Stack, id: &str, v: [f32; 2]) -> Result<()> {
    f.db.run_on_stack(
        &format!(
            "?[id, at, v] <- [['{id}', 'ASSERT', [{}, {}]]] :put vrec {{id, at => v}}",
            v[0], v[1]
        ),
        Default::default(),
        stack,
        None,
        ScriptMutability::Mutable,
    )?;
    Ok(())
}

/// The `k` nearest neighbours of a probe, through a stack.
fn neighbours(f: &Fixture, stack: &Stack) -> Result<BTreeSet<String>> {
    let rows = f.db.run_on_stack(
        "?[id] := ~vrec:idx{id | query: q, k: 10, ef: 50}, q = vec([1.0, 1.0])",
        Default::default(),
        stack,
        None,
        ScriptMutability::Immutable,
    )?;
    Ok(rows.rows.iter().map(|r| super::as_str(&r[0])).collect())
}

/// Vectors written in a branch, queried through the stack, then merged down with the
/// index dropped and rebuilt. Recall after the rebuild matches what the stack query returned.
#[test]
fn an_index_survives_a_branch_cycle_by_being_rebuilt() -> Result<()> {
    let f = Fixture::new()?;
    f.db
        .run_script(SCHEMA, Default::default(), ScriptMutability::Mutable)?;
    f.db
        .run_script(INDEX, Default::default(), ScriptMutability::Mutable)?;

    for (id, v) in [("base1", [1.0, 1.0]), ("base2", [2.0, 2.0])] {
        put(&f, &base(), id, v)?;
    }

    f.db.create_layer("work")?;
    let fork = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    for (id, v) in [("work1", [1.1, 1.1]), ("work2", [3.0, 3.0])] {
        put(&f, &work, id, v)?;
    }

    // Traversal through the stack sees both layers' vectors as one graph: the branch's
    // inserts linked its new nodes into the base's neighbour lists, and the base's edges are
    // still visible underneath them.
    let through_stack = neighbours(&f, &work)?;
    assert_eq!(
        through_stack,
        ["base1", "base2", "work1", "work2"]
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>(),
        "traversal through the stack lost a layer"
    );

    // The base, meanwhile, knows nothing of the branch.
    let before_merge = neighbours(&f, &base())?;
    assert!(!before_merge.contains("work1"));

    // Merge down. The index relation is not copied: only the records are.
    let stats = f.db.flatten(&vec![LayerRef::new("work")], &base(), true)?;
    assert_eq!(stats.rows_copied, 2, "only the records should be copied");

    // Rebuild in the destination, and recall matches the pre-merge stack query.
    f.db.run_script(
        "::hnsw drop vrec:idx",
        Default::default(),
        ScriptMutability::Mutable,
    )?;
    f.db
        .run_script(INDEX, Default::default(), ScriptMutability::Mutable)?;
    f.db.drop_layer("work")?;

    assert_eq!(neighbours(&f, &base())?, through_stack);
    Ok(())
}

/// Retract a record through `stack`.
fn retract(f: &Fixture, stack: &Stack, id: &str, v: [f32; 2]) -> Result<()> {
    f.db.run_on_stack(
        &format!(
            "?[id, at, v] <- [['{id}', 'RETRACT', [{}, {}]]] :put vrec {{id, at => v}}",
            v[0], v[1]
        ),
        Default::default(),
        stack,
        None,
        ScriptMutability::Mutable,
    )?;
    Ok(())
}

/// The `k` nearest *live* records, through a stack.
fn live_neighbours(f: &Fixture, stack: &Stack) -> Result<BTreeSet<String>> {
    let rows = f.db.run_on_stack(
        "?[id] := ~vrec:idx{id | query: q, k: 10, ef: 50, validity: 'NOW'}, q = vec([1.0, 1.0])",
        Default::default(),
        stack,
        None,
        ScriptMutability::Immutable,
    )?;
    Ok(rows.rows.iter().map(|r| super::as_str(&r[0])).collect())
}

/// Every version of a record is its own node in the graph, and a retraction writes a version
/// rather than removing one. So a branch that retracts a record still has the record's older
/// versions in reach, and only a search that asks about liveness stops returning it.
#[test]
fn a_retraction_in_a_branch_hides_the_record_from_that_branch_only() -> Result<()> {
    let f = Fixture::new()?;
    f.db
        .run_script(SCHEMA, Default::default(), ScriptMutability::Mutable)?;
    f.db
        .run_script(INDEX, Default::default(), ScriptMutability::Mutable)?;
    for (id, v) in [("a", [1.0, 1.0]), ("b", [1.2, 1.2]), ("c", [1.4, 1.4])] {
        put(&f, &base(), id, v)?;
    }

    f.db.create_layer("work")?;
    let fork = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    retract(&f, &work, "b", [1.2, 1.2])?;

    let all = ["a", "b", "c"]
        .iter()
        .map(|s| s.to_string())
        .collect::<BTreeSet<_>>();
    // The retraction is only a newer version, so the graph still reaches `b` from either side.
    assert_eq!(neighbours(&f, &work)?, all);
    assert_eq!(neighbours(&f, &base())?, all);

    assert_eq!(
        live_neighbours(&f, &work)?,
        ["a", "c"].iter().map(|s| s.to_string()).collect(),
        "the branch still sees the record it retracted"
    );
    assert_eq!(
        live_neighbours(&f, &base())?,
        all,
        "a branch retraction reached the base"
    );
    Ok(())
}

/// A bound cuts the records off at a sequence but not the index, whose keys carry no trailing
/// validity for a window to select on. The graph therefore names records the stack cannot
/// read. Those nodes drop out of the walk; the search is not an error and does not lose the
/// records that are still there.
#[test]
fn a_bounded_stack_skips_the_nodes_it_cannot_resolve() -> Result<()> {
    let f = Fixture::new()?;
    f.db
        .run_script(SCHEMA, Default::default(), ScriptMutability::Mutable)?;
    f.db
        .run_script(INDEX, Default::default(), ScriptMutability::Mutable)?;
    for i in 0..12 {
        put(&f, &base(), &format!("b{i:02}"), [i as f32, 0.0])?;
    }

    f.db.create_layer("work")?;
    let fork = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    put(&f, &work, "w0", [0.5, 1.0])?;

    // The base moves on. Its new rows join the graph, which the bound does not hide.
    for i in 12..24 {
        put(&f, &base(), &format!("b{i:02}"), [i as f32, 0.0])?;
    }

    let found = neighbours(&f, &work)?;
    let visible: BTreeSet<String> = f
        .db
        .run_on_stack(
            "?[id] := *vrec{id @ 'NOW'}",
            Default::default(),
            &work,
            None,
            ScriptMutability::Immutable,
        )?
        .rows
        .iter()
        .map(|r| super::as_str(&r[0]))
        .collect();

    assert!(
        found.is_subset(&visible),
        "returned a record the stack cannot read: {:?}",
        &found - &visible
    );
    for id in ["b00", "b01", "w0"] {
        assert!(found.contains(id), "lost {id}; got {found:?}");
    }
    Ok(())
}
