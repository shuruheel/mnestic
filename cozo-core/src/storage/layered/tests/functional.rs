/*
 * Composite scenarios: branch-shaped work built only from the public API, so these double
 * as documentation of what a consumer is expected to do.
 */

use miette::Result;

use super::{base, frontier, Fixture};
use crate::storage::layered::{LayerRef, Stack};

/// The basic branch shape, end to end: a scratch layer over a bounded base.
///
/// The isolation is asymmetric on purpose. `work` is blind to `base` by *bound*; `base` is
/// blind to `work` by *stack membership*, and needs no bound of its own.
#[test]
fn forked_reads_are_isolated_in_both_directions() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "established", "v1")?;

    f.db.create_layer("work")?;
    let fork = f.seq();
    let branch: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];

    f.assert_rec(&branch, "scratch", "v2")?;

    // The branch sees the base as of the fork, plus its own work.
    assert_eq!(
        f.live(&branch, None)?,
        frontier(&[("established", "v1"), ("scratch", "v2")])
    );
    // The base is unaware the branch exists.
    assert_eq!(f.live(&base(), None)?, frontier(&[("established", "v1")]));
    Ok(())
}

/// The parent keeps moving after the fork, and none of it reaches the branch.
#[test]
fn the_bound_seals_the_parent() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "shared", "v1")?;

    f.db.create_layer("work")?;
    let fork = f.seq();
    let branch: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];

    f.assert_rec(&base(), "parent-only", "v2")?;

    assert_eq!(f.live(&branch, None)?, frontier(&[("shared", "v1")]));
    assert_eq!(
        f.live(&base(), None)?,
        frontier(&[("shared", "v1"), ("parent-only", "v2")])
    );
    Ok(())
}

/// The half of P6 that hurts if it regresses: a parent's post-fork *delete* must not delete
/// the record inside the fork.
#[test]
fn a_post_fork_parent_delete_does_not_reach_the_branch() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "shared", "v1")?;

    f.db.create_layer("work")?;
    let fork = f.seq();
    let branch: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];

    f.retract_rec(&base(), "shared")?;

    assert_eq!(f.live(&base(), None)?, frontier(&[]));
    assert_eq!(f.live(&branch, None)?, frontier(&[("shared", "v1")]));
    Ok(())
}

/// A branch deletes a record it does not own. The retraction shadows the base's row through
/// this stack and only through this stack.
#[test]
fn a_branch_deletes_a_record_it_does_not_own() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "doomed", "v1")?;
    f.assert_rec(&base(), "spared", "v1")?;

    f.db.create_layer("work")?;
    let fork = f.seq();
    let branch: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];

    f.retract_rec(&branch, "doomed")?;

    assert_eq!(f.live(&branch, None)?, frontier(&[("spared", "v1")]));
    assert_eq!(
        f.live(&base(), None)?,
        frontier(&[("doomed", "v1"), ("spared", "v1")])
    );
    Ok(())
}

/// A branch of a branch. Each layer answers only to its own window: a base row written after
/// the *first* fork stays invisible even though it precedes the second.
#[test]
fn a_lineage_bounds_each_ancestor_at_its_own_fork() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "root", "v1")?;

    f.db.create_layer("work")?;
    let fork1 = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork1)];
    f.assert_rec(&work, "on-work", "v2")?;

    // The parent moves on between the two forks. `work2` must not acquire this.
    f.assert_rec(&base(), "after-fork1", "v3")?;

    f.db.create_layer("work2")?;
    let fork2 = f.seq();
    let work2: Stack = vec![
        LayerRef::new("work2"),
        LayerRef::bounded("work", fork2),
        LayerRef::bounded("default", fork1),
    ];
    f.assert_rec(&work2, "on-work2", "v4")?;

    assert_eq!(
        f.live(&work2, None)?,
        frontier(&[("root", "v1"), ("on-work", "v2"), ("on-work2", "v4")])
    );
    assert_eq!(
        f.live(&work, None)?,
        frontier(&[("root", "v1"), ("on-work", "v2")])
    );
    Ok(())
}

/// A record retracted on a branch and re-introduced above it, identically, is live again.
#[test]
fn a_record_can_be_retracted_and_re_introduced_up_the_lineage() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;

    f.db.create_layer("work")?;
    let fork1 = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork1)];
    f.retract_rec(&work, "k")?;
    assert_eq!(f.live(&work, None)?, frontier(&[]));

    f.db.create_layer("work2")?;
    let fork2 = f.seq();
    let work2: Stack = vec![
        LayerRef::new("work2"),
        LayerRef::bounded("work", fork2),
        LayerRef::bounded("default", fork1),
    ];
    f.assert_rec(&work2, "k", "v1")?;

    assert_eq!(f.live(&work2, None)?, frontier(&[("k", "v1")]));
    // Dropping the re-introducing layer from the stack leaves it retracted again.
    assert_eq!(f.live(&work, None)?, frontier(&[]));
    Ok(())
}

/// A changeset is a windowed read of one layer: "what changed", never "what is".
#[test]
fn a_changeset_is_a_windowed_read_of_one_layer() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "before", "v1")?;
    let commit_start = f.seq();
    f.assert_rec(&base(), "during", "v2")?;
    let commit_end = f.seq();
    f.assert_rec(&base(), "after", "v3")?;

    let changeset: Stack = vec![LayerRef::windowed(
        "default",
        Some(commit_start),
        Some(commit_end),
    )];
    assert_eq!(f.live(&changeset, None)?, frontier(&[("during", "v2")]));

    // The floor is exclusive and the ceiling inclusive, so consecutive windows tile exactly.
    let earlier: Stack = vec![LayerRef::bounded("default", commit_start)];
    assert_eq!(f.live(&earlier, None)?, frontier(&[("before", "v1")]));
    Ok(())
}

/// The plain cycle: branch, work, merge down, drop. The base ends up holding exactly the
/// base plus the work.
#[test]
fn a_branch_merges_down_and_is_dropped() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "kept", "v0")?;
    f.assert_rec(&base(), "doomed", "v1")?;

    f.db.create_layer("work")?;
    let fork = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    f.assert_rec(&work, "added", "v2")?;
    f.retract_rec(&work, "doomed")?;

    let stats = f.db.flatten(&vec![LayerRef::new("work")], &base(), true)?;
    assert_eq!(stats.rows_copied, 2);
    f.db.drop_layer("work")?;

    assert_eq!(
        f.live(&base(), None)?,
        frontier(&[("kept", "v0"), ("added", "v2")])
    );
    assert_eq!(f.db.list_layers()?.len(), 1);
    Ok(())
}

/// A three-deep lineage merged leaf-first, verifying every intermediate frontier.
#[test]
fn a_lineage_merges_leaf_first() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "root", "v0")?;

    f.db.create_layer("work")?;
    let fork1 = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork1)];
    f.assert_rec(&work, "on-work", "v1")?;

    f.db.create_layer("work2")?;
    let fork2 = f.seq();
    let work2: Stack = vec![
        LayerRef::new("work2"),
        LayerRef::bounded("work", fork2),
        LayerRef::bounded("default", fork1),
    ];
    f.assert_rec(&work2, "on-work2", "v2")?;

    // Leaf into its parent.
    f.db.flatten(&vec![LayerRef::new("work2")], &work, true)?;
    f.db.drop_layer("work2")?;
    assert_eq!(
        f.live(&work, None)?,
        frontier(&[("root", "v0"), ("on-work", "v1"), ("on-work2", "v2")])
    );

    // Parent into the base.
    f.db.flatten(&vec![LayerRef::new("work")], &base(), true)?;
    f.db.drop_layer("work")?;
    assert_eq!(
        f.live(&base(), None)?,
        frontier(&[("root", "v0"), ("on-work", "v1"), ("on-work2", "v2")])
    );
    Ok(())
}

/// Merging a layer down and dropping it while a child branch still forks off it. The
/// child's next stack construction must fail loudly rather than read something wrong.
#[test]
fn dropping_a_layer_under_a_live_child_fails_loudly() -> Result<()> {
    let f = Fixture::new()?;
    f.db.create_layer("work")?;
    let fork1 = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork1)];
    f.assert_rec(&work, "on-work", "v1")?;

    f.db.create_layer("work2")?;
    let fork2 = f.seq();
    let work2: Stack = vec![
        LayerRef::new("work2"),
        LayerRef::bounded("work", fork2),
        LayerRef::bounded("default", fork1),
    ];
    f.assert_rec(&work2, "on-work2", "v2")?;

    f.db.flatten(&vec![LayerRef::new("work")], &base(), true)?;
    f.db.drop_layer("work")?;

    let err = f.live(&work2, None).unwrap_err();
    assert!(format!("{err:?}").contains("no such layer"), "{err:?}");
    // Re-parenting is the consumer's move, and it is available: the work merged down.
    assert_eq!(f.live(&base(), None)?, frontier(&[("on-work", "v1")]));
    Ok(())
}

/// Cherry-pick one commit (a `(layer, sequence interval)` window) into another branch,
/// and again, idempotently.
#[test]
fn a_commit_can_be_cherry_picked() -> Result<()> {
    let f = Fixture::new()?;
    let fork = f.seq();
    f.db.create_layer("source")?;
    f.db.create_layer("target")?;
    let source: Stack = vec![LayerRef::new("source"), LayerRef::bounded("default", fork)];
    let target: Stack = vec![LayerRef::new("target"), LayerRef::bounded("default", fork)];

    f.assert_rec(&source, "before", "v0")?;
    let commit_start = f.seq();
    f.assert_rec(&source, "wanted", "v1")?;
    let commit_end = f.seq();
    f.assert_rec(&source, "after", "v2")?;

    let commit: Stack = vec![LayerRef::windowed(
        "source",
        Some(commit_start),
        Some(commit_end),
    )];
    let first = f.db.flatten(&commit, &target, true)?;
    assert_eq!(first.rows_copied, 1);
    assert_eq!(f.live(&target, None)?, frontier(&[("wanted", "v1")]));

    let second = f.db.flatten(&commit, &target, true)?;
    assert_eq!(second.rows_copied, 0);
    assert_eq!(second.rows_deduped, 1);
    assert_eq!(f.live(&target, None)?, frontier(&[("wanted", "v1")]));
    Ok(())
}

/// Cherry-picking a delete: it bites where the target holds the record, and is a clean
/// no-op where the target never saw it.
#[test]
fn a_delete_can_be_cherry_picked() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "shared", "v1")?;
    let fork = f.seq();
    f.db.create_layer("source")?;
    f.db.create_layer("has-it")?;
    f.db.create_layer("never-saw-it")?;

    let source: Stack = vec![LayerRef::new("source"), LayerRef::bounded("default", fork)];
    f.retract_rec(&source, "shared")?;
    let pick: Stack = vec![LayerRef::new("source")];

    let has_it: Stack = vec![LayerRef::new("has-it"), LayerRef::bounded("default", fork)];
    let bit = f.db.flatten(&pick, &has_it, true)?;
    assert_eq!(bit.rows_copied, 1);
    assert_eq!(f.live(&has_it, None)?, frontier(&[]));

    // A branch whose lineage never held the record: nothing to delete, nothing written.
    let stranger: Stack = vec![LayerRef::new("never-saw-it")];
    let missed = f.db.flatten(&pick, &stranger, true)?;
    assert_eq!(missed.rows_copied, 0);
    assert_eq!(missed.tombstones_dropped, 1);
    Ok(())
}

/// Revert: cherry-pick the inverse of a window, retracting what it asserted and re-asserting
/// what it retracted, and the frontier returns to what it was.
#[test]
fn a_window_can_be_reverted() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "existing", "v0")?;
    let before = f.seq();

    f.assert_rec(&base(), "added", "v1")?;
    f.retract_rec(&base(), "existing")?;
    let after = f.seq();
    assert_eq!(f.live(&base(), None)?, frontier(&[("added", "v1")]));

    // The changeset of the window, with the assertion flags the revert must invert.
    let window: Stack = vec![LayerRef::windowed("default", Some(before), Some(after))];
    let changes = f.versions(&window)?;
    assert_eq!(changes.len(), 2);

    for change in changes {
        if change.assertive {
            f.retract_rec(&base(), &change.id)?;
        } else {
            // Re-assert from the pre-image: the value is immutable, so the one the record
            // always had is the only one that could be written back.
            let pre_image = f
                .versions(&vec![LayerRef::bounded("default", before)])?
                .into_iter()
                .find(|v| v.id == change.id && v.assertive)
                .expect("a retraction must have had something to retract");
            f.assert_rec(&base(), &change.id, &pre_image.val)?;
        }
    }

    assert_eq!(f.live(&base(), None)?, frontier(&[("existing", "v0")]));
    assert_eq!(f.live(&base(), Some(before))?, frontier(&[("existing", "v0")]));
    Ok(())
}

/// Reset: re-fork at an earlier point. Heads never move backwards; stacks do.
#[test]
fn a_branch_can_be_reset_by_re_forking() -> Result<()> {
    let f = Fixture::new()?;
    let fork = f.seq();
    f.db.create_layer("work")?;
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    f.assert_rec(&work, "keep", "v1")?;
    let reset_point = f.seq();
    f.assert_rec(&work, "discard", "v2")?;

    f.db.create_layer("work-reset")?;
    let reset: Stack = vec![
        LayerRef::new("work-reset"),
        LayerRef::bounded("work", reset_point),
        LayerRef::bounded("default", fork),
    ];
    assert_eq!(f.live(&reset, None)?, frontier(&[("keep", "v1")]));

    f.assert_rec(&reset, "redone", "v3")?;
    assert_eq!(
        f.live(&reset, None)?,
        frontier(&[("keep", "v1"), ("redone", "v3")])
    );
    Ok(())
}

/// The same record cherry-picked into two branches, both merged down: one copy survives
/// and the second merge dedupes.
#[test]
fn convergent_picks_merge_to_one_copy() -> Result<()> {
    let f = Fixture::new()?;
    let fork = f.seq();
    f.db.create_layer("a")?;
    f.db.create_layer("b")?;
    let a: Stack = vec![LayerRef::new("a"), LayerRef::bounded("default", fork)];
    let b: Stack = vec![LayerRef::new("b"), LayerRef::bounded("default", fork)];

    f.assert_rec(&a, "shared", "v1")?;
    f.assert_rec(&b, "shared", "v1")?;

    let first = f.db.flatten(&vec![LayerRef::new("a")], &base(), true)?;
    assert_eq!(first.rows_copied, 1);
    let second = f.db.flatten(&vec![LayerRef::new("b")], &base(), true)?;
    assert_eq!(second.rows_copied, 0);
    assert_eq!(second.rows_deduped, 1);

    assert_eq!(f.versions(&base())?.len(), 1);
    assert_eq!(f.live(&base(), None)?, frontier(&[("shared", "v1")]));
    Ok(())
}

/// A long chain of fork/merge cycles. The frontier stays correct throughout and the
/// stack depth returns to one after every merge-down, so the merge-promptly discipline holds
/// as an invariant rather than an assumption.
#[test]
fn a_long_chain_of_cycles_stays_correct() -> Result<()> {
    let f = Fixture::new()?;
    let mut expected: Vec<(String, String)> = vec![];

    for round in 0..8 {
        let name = format!("work{round}");
        f.db.create_layer(name.as_str())?;
        let fork = f.seq();
        let work: Stack = vec![
            LayerRef::new(name.as_str()),
            LayerRef::bounded("default", fork),
        ];

        let id = format!("k{round}");
        f.assert_rec(&work, &id, "v")?;
        expected.push((id, "v".to_string()));

        // Every other round also deletes the record the previous round added.
        if round % 2 == 1 {
            let doomed = format!("k{}", round - 1);
            f.retract_rec(&work, &doomed)?;
            expected.retain(|(k, _)| *k != doomed);
        }

        f.db.flatten(&vec![LayerRef::new(name.as_str())], &base(), true)?;
        f.db.drop_layer(name.as_str())?;

        let want: std::collections::BTreeMap<String, String> = expected.iter().cloned().collect();
        assert_eq!(f.live(&base(), None)?, want, "after round {round}");
        assert_eq!(f.db.list_layers()?.len(), 1, "after round {round}");
    }
    Ok(())
}

/// Time travel composes with the structural bound rather than fighting it: the effective
/// ceiling is the smaller of the two.
#[test]
fn a_query_bound_composes_with_a_layer_bound() -> Result<()> {
    let f = Fixture::new()?;
    let first = f.assert_rec(&base(), "a", "v1")?;
    f.assert_rec(&base(), "b", "v2")?;
    let fork = f.seq();
    f.assert_rec(&base(), "c", "v3")?;

    let bounded: Stack = vec![LayerRef::bounded("default", fork)];
    // `at` above the bound: the bound wins.
    assert_eq!(
        f.live(&bounded, Some(f.seq()))?,
        frontier(&[("a", "v1"), ("b", "v2")])
    );
    // `at` below the bound: `at` wins.
    assert_eq!(f.live(&bounded, Some(first))?, frontier(&[("a", "v1")]));
    Ok(())
}
