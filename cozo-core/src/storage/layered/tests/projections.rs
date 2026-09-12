/*
 * Cached graph projections through a stack.
 *
 * A projection is an in-memory adjacency built from a relation and reused across algorithm
 * calls. Its guarantee is that it never serves data differing from what the consuming
 * transaction's own scan would return, and the protocol behind that reasons entirely about
 * *mutation*: a relation's content version changes when someone writes it.
 *
 * A stack breaks that assumption without writing anything. Two transactions over the same
 * relation, no write between them, see different content because they compose different
 * layers. The cache therefore keys on the view as well, and these tests pin both halves: that
 * a stack is never served another stack's graph, and that keying on the view did not simply
 * turn caching off.
 */

use miette::Result;

use super::{base, Fixture};
use crate::runtime::db::ScriptMutability::{Immutable, Mutable};
use crate::storage::layered::{LayerRef, Stack};

/// An edge relation that a multi-layer stack is allowed to read: its last key column is a
/// validity, so it can express a cross-layer retraction.
const EDGES: &str = ":create knows {a: Int, b: Int, at: Validity}";

fn link(f: &Fixture, stack: &Stack, a: i64, b: i64) -> Result<()> {
    f.db.run_on_stack(
        &format!("?[a, b, at] <- [[{a}, {b}, 'ASSERT']] :put knows {{a, b, at}}"),
        Default::default(),
        stack,
        None,
        Mutable,
    )?;
    Ok(())
}

/// Nodes reachable through the named projection, which is what an algorithm consuming it sees.
fn nodes_via_projection(f: &Fixture, stack: &Stack) -> Result<usize> {
    Ok(f.db
        .run_on_stack(
            "?[n, c] <~ ConnectedComponents(graph: 'g')",
            Default::default(),
            stack,
            None,
            Immutable,
        )?
        .rows
        .len())
}

/// Two branches of one relation, and a projection over it.
fn two_branches(f: &Fixture) -> Result<(Stack, Stack, i64)> {
    f.db.run_script(EDGES, Default::default(), Mutable)?;
    link(f, &base(), 1, 2)?;
    let fork = f.seq();
    f.db.create_layer("work")?;
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    link(f, &work, 3, 4)?;
    f.db.run_on_stack(
        "::graph create g {edges: knows}",
        Default::default(),
        &base(),
        None,
        Mutable,
    )?;
    Ok((base(), work, fork))
}

/// The base's graph is one edge over two nodes; the branch's is that plus its own, over four.
/// Each stack must get its own, whichever asks first.
#[test]
fn a_stack_is_never_served_another_stacks_graph() -> Result<()> {
    let f = Fixture::new()?;
    let (base_stack, work, _) = two_branches(&f)?;

    // Base first, so the branch is the one that would have taken a stale hit.
    assert_eq!(nodes_via_projection(&f, &base_stack)?, 2);
    assert_eq!(
        nodes_via_projection(&f, &work)?,
        4,
        "the branch was served the base's graph"
    );

    // And in the other order, on a fresh store: whoever builds first must not poison the other.
    let g = Fixture::new()?;
    let (base_stack, work, _) = two_branches(&g)?;
    assert_eq!(nodes_via_projection(&g, &work)?, 4);
    assert_eq!(
        nodes_via_projection(&g, &base_stack)?,
        2,
        "the base was served the branch's graph"
    );
    Ok(())
}

/// Keying on the view must not amount to switching caching off: a second query on the same
/// stack still reuses what the first one built.
#[test]
fn a_stack_reuses_its_own_cached_projection() -> Result<()> {
    let f = Fixture::new()?;
    let (base_stack, work, _) = two_branches(&f)?;

    nodes_via_projection(&f, &base_stack)?;
    nodes_via_projection(&f, &work)?;
    let after_first_pass = f.db.graph_projections.build_count();

    nodes_via_projection(&f, &base_stack)?;
    nodes_via_projection(&f, &work)?;
    assert_eq!(
        f.db.graph_projections.build_count(),
        after_first_pass,
        "a repeated query on an unchanged stack rebuilt instead of hitting the cache"
    );
    Ok(())
}

/// A layer name is reusable, so a dropped and recreated layer is a different layer holding
/// different content. Nothing is written to the relation across the recreate, so the mutation
/// half of the protocol cannot tell the two apart and would serve the dead layer's graph. The
/// generation carried in the view identity is the only thing that distinguishes them.
#[test]
fn a_recreated_layer_does_not_inherit_the_old_ones_graph() -> Result<()> {
    let f = Fixture::new()?;
    let (_, work, fork) = two_branches(&f)?;
    assert_eq!(nodes_via_projection(&f, &work)?, 4, "the branch's own graph");

    // Replace the layer. Deliberately write nothing afterwards: a write would bump the
    // relation's content version and invalidate the entry through the mutation axis, which
    // would hide whether the view identity did its job.
    f.db.drop_layer("work")?;
    f.db.create_layer("work")?;
    // The same name and the same window as before, so the generation is the only thing that
    // differs between the dead layer's view identity and the new one's.
    let reborn: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];

    assert_eq!(
        nodes_via_projection(&f, &reborn)?,
        2,
        "an empty new layer over the base should see only the base's edge; it was served the \
         graph of the layer it replaced"
    );
    Ok(())
}

/// Distinct stacks must have distinct identities even when a layer name looks like the
/// rendering of another stack.
///
/// The identity is structured, so this cannot fail by construction. It is here because an
/// earlier version encoded the stack into one string, and a layer named after that encoding's
/// separators reproduced another stack's identity exactly, after a reopen. The test pins the
/// representation against regressing to anything with an encoding to forge.
#[test]
fn a_layer_name_cannot_forge_another_stacks_identity() -> Result<()> {
    use crate::storage::layered::new_cozo_layered;
    use crate::storage::{Storage, StorageView, StoreTx};

    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = new_cozo_layered(dir.path())?;
        db.create_layer("a#0;b")?;
        db.create_layer("a")?;
        db.create_layer("b")?;
    }
    let db = new_cozo_layered(dir.path())?;
    let view_of = |stack: &Stack| -> Result<StorageView> {
        let spec = db.db.resolve(stack)?;
        let bound = db.db.with_stack(spec);
        let tx = bound.transact(false)?;
        Ok(tx.storage_view())
    };

    let forged: Stack = vec![LayerRef::new("a#0;b")];
    let genuine: Stack = vec![LayerRef::new("a"), LayerRef::new("b")];
    assert_ne!(
        view_of(&forged)?,
        view_of(&genuine)?,
        "a layer name reproduced another stack's view identity"
    );
    Ok(())
}

/// Two stacks over the same layers at the same generations, differing only in what their
/// windows admit, are different views and must not share a cached graph.
#[test]
fn stacks_differing_only_by_window_are_different_views() -> Result<()> {
    let f = Fixture::new()?;
    f.db.run_script(EDGES, Default::default(), Mutable)?;
    link(&f, &base(), 1, 2)?;
    let early = f.seq();
    link(&f, &base(), 3, 4)?;
    f.db.run_on_stack(
        "::graph create g {edges: knows}",
        Default::default(),
        &base(),
        None,
        Mutable,
    )?;

    // Same layer, same generation; only the ceiling differs.
    let clipped: Stack = vec![LayerRef::bounded("default", early)];
    let whole: Stack = vec![LayerRef::new("default")];
    assert_eq!(nodes_via_projection(&f, &whole)?, 4);
    assert_eq!(
        nodes_via_projection(&f, &clipped)?,
        2,
        "a bounded stack was served the unbounded stack's graph"
    );
    Ok(())
}

/// A lone unwindowed default layer is the whole store, which is the view every non-layered
/// engine presents. Reporting it as such costs nothing to build and lets the ordinary path
/// share cache entries with plain `run_script`, which reads exactly the same rows.
#[test]
fn the_plain_default_stack_is_the_global_view() -> Result<()> {
    use crate::storage::{Storage, StorageView, StoreTx};

    let f = Fixture::new()?;
    f.db.run_script(EDGES, Default::default(), Mutable)?;
    link(&f, &base(), 1, 2)?;
    let fork = f.seq();
    f.db.create_layer("work")?;

    let view_of = |stack: &Stack| -> Result<StorageView> {
        let spec = f.db.db.resolve(stack)?;
        let bound = f.db.db.with_stack(spec);
        let tx = bound.transact(false)?;
        Ok(tx.storage_view())
    };

    assert_eq!(view_of(&base())?, StorageView::undivided());
    // Anything else is a composed view, distinct from GLOBAL and from each other.
    let bounded = view_of(&vec![LayerRef::bounded("default", fork)])?;
    let stacked = view_of(&vec![
        LayerRef::new("work"),
        LayerRef::bounded("default", fork),
    ])?;
    assert_ne!(bounded, StorageView::undivided());
    assert_ne!(stacked, StorageView::undivided());
    assert_ne!(bounded, stacked);
    Ok(())
}

/// The behavioural half of the above: a projection built through plain `run_script` is reused
/// by an explicit read of the same default stack, rather than rebuilt under a second identity.
#[test]
fn plain_and_explicit_reads_of_the_default_share_a_projection() -> Result<()> {
    let f = Fixture::new()?;
    f.db.run_script(EDGES, Default::default(), Mutable)?;
    link(&f, &base(), 1, 2)?;
    f.db.run_script(
        "::graph create g {edges: knows}",
        Default::default(),
        Mutable,
    )?;

    f.db.run_script(
        "?[n, c] <~ ConnectedComponents(graph: 'g')",
        Default::default(),
        Immutable,
    )?;
    let after_plain = f.db.graph_projections.build_count();

    nodes_via_projection(&f, &base())?;
    assert_eq!(
        f.db.graph_projections.build_count(),
        after_plain,
        "an explicit read of the default stack rebuilt what plain run_script had already built"
    );
    Ok(())
}

/// Every layer gets its own incarnation, including the ones already on disk when the store
/// opens. A shared value would make two layers indistinguishable to anything keyed on the
/// view, which is the whole point of carrying one.
#[test]
fn layers_present_at_open_get_distinct_incarnations() -> Result<()> {
    use crate::storage::layered::new_cozo_layered;
    use crate::storage::{Storage, StorageView, StoreTx};

    let dir = tempfile::TempDir::new().unwrap();
    {
        let db = new_cozo_layered(dir.path())?;
        db.create_layer("one")?;
        db.create_layer("two")?;
    }
    let db = new_cozo_layered(dir.path())?;
    let view_of = |stack: &Stack| -> Result<StorageView> {
        let spec = db.db.resolve(stack)?;
        let bound = db.db.with_stack(spec);
        let tx = bound.transact(false)?;
        Ok(tx.storage_view())
    };

    assert_ne!(
        view_of(&vec![LayerRef::new("one")])?,
        view_of(&vec![LayerRef::new("two")])?,
        "two layers that existed before the store opened share an incarnation"
    );
    Ok(())
}
