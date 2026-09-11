/*
 * Value immutability.
 *
 * A key's value is immutable; its visibility is not. Everything merge soundness rests on is in
 * this suite: with values that never change, merging two layers is a set union.
 */

use miette::Result;

use super::{base, frontier, Fixture};
use crate::storage::layered::{LayerRef, Stack};

fn branch(f: &Fixture, name: &str) -> Result<Stack> {
    f.db.create_layer(name)?;
    let fork = f.seq();
    Ok(vec![LayerRef::new(name), LayerRef::bounded("default", fork)])
}

/// Re-asserting the identical value is accepted and changes nothing.
#[test]
fn an_identical_re_assert_is_accepted() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    f.assert_rec(&base(), "k", "v1")?;
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));
    Ok(())
}

/// A different value under a live key in the same layer is rejected.
#[test]
fn a_differing_re_assert_is_rejected() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let err = f.assert_rec(&base(), "k", "v2").unwrap_err();
    assert!(
        format!("{err:?}").contains("never updated"),
        "unexpected error: {err:?}"
    );
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));
    Ok(())
}

/// The check traverses the whole stack, not just the top layer: a value held only in a
/// lower layer still forbids a differing assert above it.
#[test]
fn a_differing_re_assert_is_rejected_across_layers() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let work = branch(&f, "work")?;
    let err = f.assert_rec(&work, "k", "v2").unwrap_err();
    assert!(format!("{err:?}").contains("never updated"), "{err:?}");
    Ok(())
}

/// Delete-then-recreate is backdoor mutability wherever the tombstone lives, and is
/// rejected just as an in-place update would be.
#[test]
fn recreating_a_retracted_key_with_a_new_value_is_rejected() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    f.retract_rec(&base(), "k")?;
    let err = f.assert_rec(&base(), "k", "v2").unwrap_err();
    assert!(format!("{err:?}").contains("never updated"), "{err:?}");
    assert_eq!(f.live(&base(), None)?, frontier(&[]));
    Ok(())
}

/// Undelete: the identical value over a tombstoned key is how a retracted record comes
/// back, whether by hand or by cherry-pick.
#[test]
fn an_identical_value_undeletes() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    f.retract_rec(&base(), "k")?;
    f.assert_rec(&base(), "k", "v1")?;
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));
    Ok(())
}

/// The invisible collision. A parent acquires a key above the branch's bound, with a
/// different value; the branch asserts it too. The write *must* succeed, because the conflicting row
/// is unknowable through this stack, and the merge must be what fails.
#[test]
fn a_collision_invisible_to_the_branch_is_caught_at_the_merge() -> Result<()> {
    let f = Fixture::new()?;
    let work = branch(&f, "work")?;

    f.assert_rec(&base(), "k", "from-base")?;
    // The branch cannot see that row: its bound predates it.
    f.assert_rec(&work, "k", "from-branch")?;

    assert_eq!(f.live(&work, None)?, frontier(&[("k", "from-branch")]));
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "from-base")]));

    let err = f
        .db
        .flatten(&vec![LayerRef::new("work")], &base(), true)
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("more than one value"),
        "unexpected error: {err:?}"
    );
    // Nothing was written: the base is exactly as it was.
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "from-base")]));
    Ok(())
}

/// A rejected assert writes nothing and poisons nothing.
#[test]
fn a_rejected_assert_leaves_the_session_usable() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    assert!(f.assert_rec(&base(), "k", "v2").is_err());

    f.assert_rec(&base(), "k2", "v3")?;
    assert_eq!(
        f.live(&base(), None)?,
        frontier(&[("k", "v1"), ("k2", "v3")])
    );
    Ok(())
}

/// Relations with no validity keep stock semantics: they never participate in a merge, so
/// nothing rests on their immutability.
#[test]
fn relations_without_validity_are_unaffected() -> Result<()> {
    let f = Fixture::new()?;
    f.db.run_script(
        ":create plain {k: Int => v: Int}",
        Default::default(),
        crate::runtime::db::ScriptMutability::Mutable,
    )?;
    f.db.run_script(
        "?[k, v] <- [[1, 10]] :put plain {k => v}",
        Default::default(),
        crate::runtime::db::ScriptMutability::Mutable,
    )?;
    f.db.run_script(
        "?[k, v] <- [[1, 20]] :put plain {k => v}",
        Default::default(),
        crate::runtime::db::ScriptMutability::Mutable,
    )?;
    let rows = f.db.run_script(
        "?[k, v] := *plain{k, v}",
        Default::default(),
        crate::runtime::db::ScriptMutability::Immutable,
    )?;
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0][1], crate::DataValue::from(20));
    Ok(())
}
