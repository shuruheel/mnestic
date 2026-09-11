/*
 * Cross-layer retraction: the subtlest correctness case in the design.
 */

use miette::Result;

use super::{base, frontier, Fixture};
use crate::storage::layered::{LayerRef, Stack};

fn branch(f: &Fixture, name: &str) -> Result<(Stack, i64)> {
    f.db.create_layer(name)?;
    let fork = f.seq();
    Ok((
        vec![LayerRef::new(name), LayerRef::bounded("default", fork)],
        fork,
    ))
}

/// A retraction written in the top layer hides the lower layer's row through this
/// stack and only through this stack. The lower layer keeps its row: a stack that omits the
/// retracting layer still sees it.
#[test]
fn a_retraction_is_scoped_to_the_stack_that_wrote_it() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let (work, fork) = branch(&f, "work")?;
    f.retract_rec(&work, "k")?;

    assert_eq!(f.live(&work, None)?, frontier(&[]));
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));

    // The row itself is untouched underneath: read the same base layer through a second stack.
    let other: Stack = vec![LayerRef::bounded("default", fork)];
    assert_eq!(f.live(&other, None)?, frontier(&[("k", "v1")]));
    Ok(())
}

/// Retract then re-assert the identical value in the same layer: live again.
#[test]
fn a_retraction_can_be_undone_in_place() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    f.retract_rec(&base(), "k")?;
    assert_eq!(f.live(&base(), None)?, frontier(&[]));

    f.assert_rec(&base(), "k", "v1")?;
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));
    Ok(())
}

/// Time travel across a retraction: a sequence captured before it still shows the row
/// live, through the very stack that now hides it.
#[test]
fn history_survives_a_retraction() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let before = f.seq();
    let (work, _) = branch(&f, "work")?;
    f.retract_rec(&work, "k")?;

    assert_eq!(f.live(&work, None)?, frontier(&[]));
    assert_eq!(f.live(&work, Some(before))?, frontier(&[("k", "v1")]));
    Ok(())
}

/// Retracting a key no layer holds is legal and harmless: a cherry-picked delete whose
/// record the target never saw. It costs a row that the next flatten's liveness check drops.
#[test]
fn retracting_the_never_existent_is_harmless() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "present", "v1")?;
    let (work, _) = branch(&f, "work")?;

    f.retract_rec(&work, "never-existed")?;

    assert_eq!(f.live(&work, None)?, frontier(&[("present", "v1")]));
    assert_eq!(f.live(&base(), None)?, frontier(&[("present", "v1")]));
    Ok(())
}

/// A retraction stamped above a layer's ceiling is not visible, so the row it would have
/// hidden emits.
#[test]
fn a_retraction_above_the_ceiling_does_not_bite() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let fork = f.seq();
    f.retract_rec(&base(), "k")?;

    assert_eq!(f.live(&base(), None)?, frontier(&[]));
    assert_eq!(
        f.live(&vec![LayerRef::bounded("default", fork)], None)?,
        frontier(&[("k", "v1")])
    );
    Ok(())
}

/// A hard delete cannot reach a lower layer, so the engine refuses it deterministically,
/// from the relation, not from where the row happens to sit.
///
/// Both attempts below fail identically: one names a row that lives in a lower layer, the
/// other a row this stack wrote itself. Deciding per-key would make the error depend on
/// storage layout, which is exactly the "surprise mid-query" the check exists to avoid.
#[test]
fn a_hard_delete_through_a_stack_is_refused() -> Result<()> {
    let f = Fixture::new()?;
    let below = f.assert_rec(&base(), "k", "v1")?;
    let (work, _) = branch(&f, "work")?;
    let above = f.assert_rec(&work, "own", "v2")?;

    for (id, stamp) in [("k", below), ("own", above)] {
        let err = rm(&f, &work, id, stamp).unwrap_err();
        assert!(
            format!("{err:?}").contains("is a retraction"),
            "unexpected error for {id}: {err:?}"
        );
    }

    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));
    // The same delete against a single-layer stack is ordinary and works.
    rm(&f, &base(), "k", below)?;
    assert_eq!(f.live(&base(), None)?, frontier(&[]));
    Ok(())
}

fn rm(f: &Fixture, stack: &Stack, id: &str, stamp: i64) -> Result<crate::NamedRows> {
    let mut params = std::collections::BTreeMap::new();
    params.insert("id".to_string(), crate::DataValue::from(id));
    params.insert(
        "at".to_string(),
        crate::DataValue::Validity(crate::Validity {
            timestamp: crate::ValidityTs(std::cmp::Reverse(stamp)),
            is_assert: std::cmp::Reverse(true),
        }),
    );
    f.db.run_on_stack(
        "?[id, at] <- [[$id, $at]] :rm rec {id, at}",
        params,
        stack,
        None,
        crate::runtime::db::ScriptMutability::Mutable,
    )
}
