/*
 * Flatten: net effect through a view, then dedupe, collision and tombstone liveness
 * against the destination.
 */

use miette::Result;

use super::{base, frontier, Fixture};
use crate::storage::layered::{LayerRef, Stack};

/// The claims on a conflict as `(layer, value)`, sorted so a test can compare them.
fn claims_of(conflict: &crate::storage::layered::flatten::FlattenConflict) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = conflict
        .claims
        .iter()
        .map(|c| (c.layer.clone(), super::as_str(&c.value[0])))
        .collect();
    out.sort();
    out
}

/// A branch over the base at the current sequence, plus the whole-layer view of it.
fn branch(f: &Fixture, name: &str) -> Result<(Stack, Stack)> {
    f.db.create_layer(name)?;
    let fork = f.seq();
    let stack = vec![LayerRef::new(name), LayerRef::bounded("default", fork)];
    let view = vec![LayerRef::new(name)];
    Ok((stack, view))
}

/// A record created and retracted inside the window contributes nothing: its net effect is
/// a tombstone, and a tombstone for a record the destination never saw is dropped.
#[test]
fn a_record_born_and_died_inside_the_view_contributes_nothing() -> Result<()> {
    let f = Fixture::new()?;
    let (work, view) = branch(&f, "work")?;
    f.assert_rec(&work, "ephemeral", "v1")?;
    f.retract_rec(&work, "ephemeral")?;

    let stats = f.db.flatten(&view, &base(), true)?;
    assert_eq!(stats.rows_copied, 0);
    assert_eq!(stats.tombstones_dropped, 1);
    assert_eq!(f.live(&base(), None)?, frontier(&[]));
    Ok(())
}

/// A retraction stamped above the view's ceiling is not in the view, so the live row is
/// what gets written: a view cannot see its own future.
#[test]
fn a_retraction_above_the_ceiling_is_not_in_the_view() -> Result<()> {
    let f = Fixture::new()?;
    let (work, _) = branch(&f, "work")?;
    f.assert_rec(&work, "k", "v1")?;
    let head = f.seq();
    f.retract_rec(&work, "k")?;

    let view: Stack = vec![LayerRef::bounded("work", head)];
    let stats = f.db.flatten(&view, &base(), true)?;
    assert_eq!(stats.rows_copied, 1);
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));
    Ok(())
}

/// A record created below the floor and retracted inside the window nets to a tombstone,
/// which is copied exactly when the destination holds the record live.
#[test]
fn a_net_tombstone_is_copied_only_where_it_bites() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let (work, view) = branch(&f, "work")?;
    f.retract_rec(&work, "k")?;

    let stats = f.db.flatten(&view, &base(), true)?;
    assert_eq!(stats.rows_copied, 1);
    assert_eq!(stats.tombstones_dropped, 0);
    assert_eq!(f.live(&base(), None)?, frontier(&[]));
    Ok(())
}

/// A record the destination already holds live, cherry-picked again: skipped, counted, and
/// the destination is unchanged.
#[test]
fn an_identical_record_dedupes() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let (work, view) = branch(&f, "work")?;
    f.assert_rec(&work, "k", "v1")?;

    let before = f.versions(&base())?;
    let stats = f.db.flatten(&view, &base(), true)?;
    assert_eq!(stats.rows_copied, 0);
    assert_eq!(stats.rows_deduped, 1);
    assert_eq!(f.versions(&base())?, before);
    Ok(())
}

/// The view's net assertion over a key the destination has tombstoned: the record returns.
#[test]
fn a_re_introduction_revives_a_retracted_record() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let (work, view) = branch(&f, "work")?;
    // The base retracts it after the fork; the branch, which cannot see that, still holds it.
    f.retract_rec(&base(), "k")?;
    f.assert_rec(&work, "k", "v1")?;
    assert_eq!(f.live(&base(), None)?, frontier(&[]));

    let stats = f.db.flatten(&view, &base(), true)?;
    assert_eq!(stats.rows_copied, 1);
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));
    Ok(())
}

/// The liveness check reads the destination *stack*, not its top layer: a tombstone whose
/// key is live further down the destination's lineage is still copied.
#[test]
fn liveness_is_checked_through_the_whole_destination_stack() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let fork = f.seq();

    // `dst` is a branch whose own layer is empty: `k` lives one layer down.
    f.db.create_layer("dst")?;
    let dst: Stack = vec![LayerRef::new("dst"), LayerRef::bounded("default", fork)];

    f.db.create_layer("src")?;
    let src_stack: Stack = vec![LayerRef::new("src"), LayerRef::bounded("default", fork)];
    f.retract_rec(&src_stack, "k")?;

    let stats = f.db.flatten(&vec![LayerRef::new("src")], &dst, true)?;
    assert_eq!(stats.rows_copied, 1);
    assert_eq!(stats.tombstones_dropped, 0);
    assert_eq!(f.live(&dst, None)?, frontier(&[]));
    // The base still holds it: the tombstone landed in `dst`'s own layer.
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));
    Ok(())
}

/// Both sides deleted the same record: the second tombstone bites nothing and is dropped.
#[test]
fn a_second_tombstone_is_dropped() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let (work, view) = branch(&f, "work")?;
    f.retract_rec(&work, "k")?;
    f.retract_rec(&base(), "k")?;

    let stats = f.db.flatten(&view, &base(), true)?;
    assert_eq!(stats.rows_copied, 0);
    assert_eq!(stats.tombstones_dropped, 1);
    assert_eq!(f.live(&base(), None)?, frontier(&[]));
    Ok(())
}

/// With restamping, the destination's history stays true: a read at a sequence captured
/// before the flatten shows nothing the flatten brought in.
#[test]
fn restamping_keeps_the_destination_history_true() -> Result<()> {
    let f = Fixture::new()?;
    let (work, view) = branch(&f, "work")?;
    let authored = f.assert_rec(&work, "k", "v1")?;
    let before_flatten = f.seq();

    let stats = f.db.flatten(&view, &base(), true)?;
    let (lo, hi) = stats.seq_range.expect("restamping assigns a sequence range");
    assert!(lo > before_flatten && hi >= lo);

    assert_eq!(f.live(&base(), Some(before_flatten))?, frontier(&[]));
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));

    // The row is in the base at the flatten's sequence, not the one it was authored at.
    let stamps: Vec<_> = f.versions(&base())?.into_iter().map(|v| v.seq).collect();
    assert_eq!(stamps, vec![lo]);
    assert!(lo != authored);
    Ok(())
}

/// Without restamping, rows keep their authoring stamps and interleave into the
/// destination's history where they were written.
#[test]
fn without_restamping_the_authoring_stamps_survive() -> Result<()> {
    let f = Fixture::new()?;
    let (work, view) = branch(&f, "work")?;
    let authored = f.assert_rec(&work, "k", "v1")?;

    let stats = f.db.flatten(&view, &base(), false)?;
    assert_eq!(stats.seq_range, None);

    let stamps: Vec<_> = f.versions(&base())?.into_iter().map(|v| v.seq).collect();
    assert_eq!(stamps, vec![authored]);
    // A read of the base as of the authoring sequence now shows the row, which is exactly the
    // historical inaccuracy restamping exists to avoid.
    assert_eq!(f.live(&base(), Some(authored))?, frontier(&[("k", "v1")]));
    Ok(())
}

/// The identical flatten run twice copies nothing the second time. This is also the
/// crash-recovery story: recovery is rerunning it.
#[test]
fn flatten_is_idempotent() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "kept", "v0")?;
    let (work, view) = branch(&f, "work")?;
    f.assert_rec(&work, "added", "v1")?;
    f.retract_rec(&work, "kept")?;

    let first = f.db.flatten(&view, &base(), true)?;
    assert_eq!(first.rows_copied, 2);
    let after_first = f.live(&base(), None)?;

    let second = f.db.flatten(&view, &base(), true)?;
    assert_eq!(second.rows_copied, 0);
    assert_eq!(second.rows_deduped, 1);
    assert_eq!(second.tombstones_dropped, 1);
    assert_eq!(f.live(&base(), None)?, after_first);
    Ok(())
}

/// An empty view writes nothing and leaves the destination alone.
#[test]
fn an_empty_view_is_a_no_op() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    let (_work, view) = branch(&f, "work")?;

    let before = f.versions(&base())?;
    let stats = f.db.flatten(&view, &base(), true)?;
    assert_eq!(stats.rows_copied, 0);
    assert_eq!(stats.rows_deduped, 0);
    assert_eq!(stats.tombstones_dropped, 0);
    assert_eq!(f.versions(&base())?, before);
    Ok(())
}

/// Flattening into a destination whose top layer already holds unmerged work (a
/// cherry-pick into an active branch) leaves that work alone, and checks against the whole
/// destination stack.
#[test]
fn flattening_into_a_dirty_head_leaves_its_work_alone() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "root", "v0")?;
    let fork = f.seq();

    f.db.create_layer("target")?;
    let target: Stack = vec![LayerRef::new("target"), LayerRef::bounded("default", fork)];
    f.assert_rec(&target, "target-work", "v1")?;

    f.db.create_layer("source")?;
    let source: Stack = vec![LayerRef::new("source"), LayerRef::bounded("default", fork)];
    f.assert_rec(&source, "picked", "v2")?;

    let stats = f.db.flatten(&vec![LayerRef::new("source")], &target, true)?;
    assert_eq!(stats.rows_copied, 1);
    assert_eq!(
        f.live(&target, None)?,
        frontier(&[("root", "v0"), ("target-work", "v1"), ("picked", "v2")])
    );
    Ok(())
}

/// A plan reports what a flatten would do, and does none of it. Running the flatten afterwards
/// reports the same counts, so a caller can act on a plan without re-deriving anything.
#[test]
fn a_plan_predicts_the_flatten_and_writes_nothing() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "shared", "v0")?;
    let (work, view) = branch(&f, "work")?;

    f.assert_rec(&work, "added", "v1")?;
    // Identical to what the base already holds: this dedupes rather than copies.
    f.assert_rec(&work, "shared", "v0")?;
    // A tombstone for a record the base never saw: this is dropped.
    f.assert_rec(&work, "ephemeral", "v2")?;
    f.retract_rec(&work, "ephemeral")?;

    let before = f.versions(&base())?;
    let plan = f.db.flatten_plan(&view, &base())?;
    assert!(plan.is_clean());
    assert_eq!(plan.stats.rows_copied, 1);
    assert_eq!(plan.stats.rows_deduped, 1);
    assert_eq!(plan.stats.tombstones_dropped, 1);
    assert!(plan.stats.bytes_copied > 0);
    // A plan commits nothing, so it occupies no sequence.
    assert_eq!(plan.stats.seq_range, None);
    assert_eq!(f.versions(&base())?, before, "the plan wrote to the base");

    let stats = f.db.flatten(&view, &base(), true)?;
    assert_eq!(stats.rows_copied, plan.stats.rows_copied);
    assert_eq!(stats.rows_deduped, plan.stats.rows_deduped);
    assert_eq!(stats.tombstones_dropped, plan.stats.tombstones_dropped);
    assert_eq!(stats.bytes_copied, plan.stats.bytes_copied);
    assert!(stats.seq_range.is_some(), "the flatten did commit");
    Ok(())
}

/// A plan reports every collision, not the first one, and identifies each in decoded terms.
#[test]
fn a_plan_reports_every_conflict() -> Result<()> {
    let f = Fixture::new()?;
    let (work, view) = branch(&f, "work")?;

    for (id, base_val, branch_val) in [
        ("a", "base-a", "branch-a"),
        ("b", "base-b", "branch-b"),
        ("c", "base-c", "branch-c"),
    ] {
        // The branch's bound predates these, so it cannot see them and the write is legal
        // on both sides. The divergence only becomes visible when the two lineages meet.
        f.assert_rec(&base(), id, base_val)?;
        f.assert_rec(&work, id, branch_val)?;
    }

    let plan = f.db.flatten_plan(&view, &base())?;
    assert!(!plan.is_clean());
    assert_eq!(plan.conflicts.len(), 3, "{:?}", plan.conflicts);

    let mut seen: Vec<(String, Vec<(String, String)>)> = plan
        .conflicts
        .iter()
        .map(|c| {
            assert_eq!(c.relation, "rec");
            // The key decodes to its columns, the identity first and the validity last.
            (super::as_str(&c.key[0]), claims_of(c))
        })
        .collect();
    seen.sort();
    let expect = |id: &str, letter: &str| {
        (
            id.to_string(),
            vec![
                ("default".to_string(), format!("base-{letter}")),
                ("work".to_string(), format!("branch-{letter}")),
            ],
        )
    };
    assert_eq!(seen, vec![expect("a", "a"), expect("b", "b"), expect("c", "c")]);
    Ok(())
}

/// The flatten that follows a conflicted plan fails, names what collided, and writes nothing.
#[test]
fn a_conflicted_flatten_names_what_collided() -> Result<()> {
    let f = Fixture::new()?;
    let (work, view) = branch(&f, "work")?;
    f.assert_rec(&base(), "k", "from-base")?;
    f.assert_rec(&work, "k", "from-branch")?;

    let before = f.versions(&base())?;
    let err = f.db.flatten(&view, &base(), true).unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("1 key(s) are claimed with more than one value"),
        "unexpected error: {msg}"
    );
    assert!(msg.contains("rec"), "the relation is not named: {msg}");
    assert!(msg.contains("from-base"), "the values are not named: {msg}");
    assert!(msg.contains("from-branch"), "the values are not named: {msg}");
    assert!(msg.contains("work"), "the layers are not named: {msg}");
    assert_eq!(f.versions(&base())?, before, "the failed flatten wrote");
    Ok(())
}

/// A plan of a view that changes nothing is clean and empty: the same no-op the flatten is.
#[test]
fn an_empty_plan_is_clean() -> Result<()> {
    let f = Fixture::new()?;
    let (_work, view) = branch(&f, "work")?;
    let plan = f.db.flatten_plan(&view, &base())?;
    assert!(plan.is_clean());
    assert_eq!(plan.stats, Default::default());
    Ok(())
}

/// Paging covers exactly what one pass covers: every item, once, in order, with no gap at a
/// page boundary and none at a relation boundary either.
#[test]
fn pages_cover_the_scan_exactly() -> Result<()> {
    use crate::runtime::db::ScriptMutability;
    use crate::storage::layered::flatten::FlattenCursor;

    let f = Fixture::new()?;
    f.db.run_script(
        ":create rec2 {id: String, at: Validity => val: String}",
        Default::default(),
        ScriptMutability::Mutable,
    )?;
    f.assert_rec(&base(), "shared", "v0")?;
    let (work, view) = branch(&f, "work")?;

    for i in 0..7 {
        f.assert_rec(&work, &format!("k{i}"), "v")?;
    }
    // Identical to the base's row: a dedupe, which must still be reported with its key.
    f.assert_rec(&work, "shared", "v0")?;
    // A second relation, so the scan has a boundary to cross mid-page.
    for i in 0..5 {
        f.db.run_on_stack(
            &format!("?[id, at, val] <- [['r{i}', 'ASSERT', 'v']] :put rec2 {{id, at => val}}"),
            Default::default(),
            &work,
            None,
            ScriptMutability::Mutable,
        )?;
    }

    let whole = f.db.flatten_page(&view, &base(), None, 1000)?;
    assert_eq!(whole.next, None, "a page past the end should not resume");
    assert_eq!(whole.items.len(), 13);

    let mut paged = vec![];
    let mut cursor: Option<FlattenCursor> = None;
    let mut pages = 0;
    loop {
        let page = f.db.flatten_page(&view, &base(), cursor.as_ref(), 2)?;
        pages += 1;
        assert!(page.items.len() <= 2);
        paged.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
        assert!(pages < 50, "paging did not terminate");
    }
    assert_eq!(paged, whole.items, "paging saw something a single pass did not");

    // The plan counts the same scan.
    let plan = f.db.flatten_plan(&view, &base())?;
    assert!(plan.is_clean());
    assert_eq!(plan.stats.rows_copied, 12);
    assert_eq!(plan.stats.rows_deduped, 1);
    Ok(())
}

/// A deduped row is reported with its key, not as an anonymous count.
#[test]
fn a_paged_dedupe_carries_its_key() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "shared", "v0")?;
    let (work, view) = branch(&f, "work")?;
    f.assert_rec(&work, "shared", "v0")?;

    let page = f.db.flatten_page(&view, &base(), None, 10)?;
    assert_eq!(page.items.len(), 1);
    match &page.items[0] {
        crate::storage::layered::flatten::FlattenItem::Dedupe { relation, key } => {
            assert_eq!(relation, "rec");
            assert_eq!(super::as_str(&key[0]), "shared");
        }
        other => panic!("expected a dedupe, got {other:?}"),
    }
    Ok(())
}

/// Conflicts appear in a page like any other item, so a diff can render them inline.
#[test]
fn a_page_reports_conflicts_inline() -> Result<()> {
    let f = Fixture::new()?;
    let (work, view) = branch(&f, "work")?;
    f.assert_rec(&base(), "k", "from-base")?;
    f.assert_rec(&work, "k", "from-branch")?;

    let page = f.db.flatten_page(&view, &base(), None, 10)?;
    assert_eq!(page.items.len(), 1);
    match &page.items[0] {
        crate::storage::layered::flatten::FlattenItem::Conflict(c) => {
            assert_eq!(c.relation, "rec");
            assert_eq!(
                claims_of(c),
                vec![
                    ("default".to_string(), "from-base".to_string()),
                    ("work".to_string(), "from-branch".to_string()),
                ]
            );
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    Ok(())
}

/// Flattening a layer into itself is refused: the scan would read the layer as it is written.
#[test]
fn a_layer_cannot_be_flattened_into_itself() -> Result<()> {
    let f = Fixture::new()?;
    let (work, view) = branch(&f, "work")?;
    f.assert_rec(&work, "k", "v")?;

    let err = f.db.flatten(&view, &work, true).unwrap_err();
    assert!(
        format!("{err:?}").contains("both the source"),
        "unexpected error: {err:?}"
    );
    Ok(())
}


/// Two sibling layers in one source view, each holding a different value for one key. Neither
/// could see the other when it wrote, so both writes were legal; stamp order is not entitled to
/// choose between them, and the disagreement is reported rather than silently resolved.
#[test]
fn siblings_in_one_source_view_do_not_resolve_by_stamp() -> Result<()> {
    let f = Fixture::new()?;
    let fork = f.seq();
    f.db.create_layer("a")?;
    f.db.create_layer("b")?;
    let a: Stack = vec![LayerRef::new("a"), LayerRef::bounded("default", fork)];
    let b: Stack = vec![LayerRef::new("b"), LayerRef::bounded("default", fork)];

    f.assert_rec(&a, "k", "from-a")?;
    f.assert_rec(&b, "k", "from-b")?;

    let both: Stack = vec![LayerRef::new("a"), LayerRef::new("b")];
    let plan = f.db.flatten_plan(&both, &base())?;
    assert!(!plan.is_clean(), "the disagreement was resolved silently");
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.stats.rows_copied, 0, "neither value may be written");
    assert_eq!(
        claims_of(&plan.conflicts[0]),
        vec![
            ("a".to_string(), "from-a".to_string()),
            ("b".to_string(), "from-b".to_string()),
        ],
        "each claim should name the layer holding it"
    );

    // And the flatten refuses rather than picking a winner.
    let before = f.versions(&base())?;
    let err = f.db.flatten(&both, &base(), true).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("more than one value"), "{msg}");
    assert!(msg.contains("a=") && msg.contains("b="), "claims are not labelled: {msg}");
    assert_eq!(f.versions(&base())?, before);
    Ok(())
}

/// Siblings that agree (the same record cherry-picked into both) are not a conflict. One
/// copy is written and the duplicate authoring is deduped away.
#[test]
fn siblings_that_agree_are_not_a_conflict() -> Result<()> {
    let f = Fixture::new()?;
    let fork = f.seq();
    f.db.create_layer("a")?;
    f.db.create_layer("b")?;
    let a: Stack = vec![LayerRef::new("a"), LayerRef::bounded("default", fork)];
    let b: Stack = vec![LayerRef::new("b"), LayerRef::bounded("default", fork)];

    f.assert_rec(&a, "k", "same")?;
    f.assert_rec(&b, "k", "same")?;

    let both: Stack = vec![LayerRef::new("a"), LayerRef::new("b")];
    let plan = f.db.flatten_plan(&both, &base())?;
    assert!(plan.is_clean());
    assert_eq!(plan.stats.rows_copied, 1, "one copy, not two");

    f.db.flatten(&both, &base(), true)?;
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "same")]));
    assert_eq!(f.versions(&base())?.len(), 1);
    Ok(())
}

/// A record asserted, retracted and re-asserted with the same value is one lineage agreeing
/// with itself, however long its version chain. The chain is never held in memory, and a
/// tombstone's value is not compared against an assertion's.
#[test]
fn a_long_version_chain_is_not_a_disagreement() -> Result<()> {
    let f = Fixture::new()?;
    let (work, view) = branch(&f, "work")?;
    for _ in 0..20 {
        f.assert_rec(&work, "k", "v")?;
        f.retract_rec(&work, "k")?;
    }
    f.assert_rec(&work, "k", "v")?;

    let plan = f.db.flatten_plan(&view, &base())?;
    assert!(plan.is_clean(), "{:?}", plan.conflicts);
    assert_eq!(plan.stats.rows_copied, 1, "only the net effect is copied");
    f.db.flatten(&view, &base(), true)?;
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v")]));
    Ok(())
}



/// A key claimed by two source layers and by the destination reports all three claims at once.
///
/// This is what makes the report complete rather than a first-check-wins verdict: the
/// destination is consulted even after the source view has already disagreed with itself, so
/// resolving one claim cannot uncover another that was hidden behind it.
#[test]
fn every_claim_is_reported_in_one_pass() -> Result<()> {
    let f = Fixture::new()?;
    let fork = f.seq();
    for name in ["a", "b", "c"] {
        f.db.create_layer(name)?;
    }
    let a: Stack = vec![LayerRef::new("a"), LayerRef::bounded("default", fork)];
    let b: Stack = vec![LayerRef::new("b"), LayerRef::bounded("default", fork)];
    let c: Stack = vec![LayerRef::new("c"), LayerRef::bounded("default", fork)];

    // Three siblings, none able to see the others, each claiming a different value.
    f.assert_rec(&a, "k", "from-a")?;
    f.assert_rec(&b, "k", "from-b")?;
    f.assert_rec(&c, "k", "from-c")?;

    // The source view holds two of the claims; the destination holds the third.
    let src: Stack = vec![LayerRef::new("a"), LayerRef::new("b")];
    let plan = f.db.flatten_plan(&src, &c)?;
    assert_eq!(plan.conflicts.len(), 1, "{:?}", plan.conflicts);
    assert_eq!(
        claims_of(&plan.conflicts[0]),
        vec![
            ("a".to_string(), "from-a".to_string()),
            ("b".to_string(), "from-b".to_string()),
            ("c".to_string(), "from-c".to_string()),
        ]
    );
    assert_eq!(plan.stats.rows_copied, 0);
    Ok(())
}

/// A destination that agrees with the source is not a claim of its own: the same value held on
/// both sides is one claim, not two.
#[test]
fn an_agreeing_destination_adds_no_claim() -> Result<()> {
    let f = Fixture::new()?;
    let fork = f.seq();
    for name in ["a", "b", "c"] {
        f.db.create_layer(name)?;
    }
    let a: Stack = vec![LayerRef::new("a"), LayerRef::bounded("default", fork)];
    let b: Stack = vec![LayerRef::new("b"), LayerRef::bounded("default", fork)];
    let c: Stack = vec![LayerRef::new("c"), LayerRef::bounded("default", fork)];

    f.assert_rec(&a, "k", "from-a")?;
    f.assert_rec(&b, "k", "from-b")?;
    // The destination happens to hold what one of the source layers holds.
    f.assert_rec(&c, "k", "from-a")?;

    let src: Stack = vec![LayerRef::new("a"), LayerRef::new("b")];
    let plan = f.db.flatten_plan(&src, &c)?;
    assert_eq!(plan.conflicts.len(), 1);
    let claims = claims_of(&plan.conflicts[0]);
    assert_eq!(claims.len(), 2, "the agreeing value was counted twice: {claims:?}");
    assert_eq!(
        claims,
        vec![
            ("a".to_string(), "from-a".to_string()),
            ("b".to_string(), "from-b".to_string()),
        ]
    );
    Ok(())
}

/// A paged row says whether it asserts, so a diff can tell an addition from a removal without
/// decoding the validity back out of the key.
#[test]
fn a_paged_row_says_whether_it_asserts() -> Result<()> {
    use crate::storage::layered::flatten::FlattenItem;

    let f = Fixture::new()?;
    f.assert_rec(&base(), "gone", "v0")?;
    let (work, view) = branch(&f, "work")?;

    f.assert_rec(&work, "added", "v1")?;
    f.retract_rec(&work, "gone")?;

    let page = f.db.flatten_page(&view, &base(), None, 10)?;
    assert_eq!(page.items.len(), 2, "{:?}", page.items);

    let mut seen: Vec<(String, bool)> = page
        .items
        .iter()
        .map(|item| match item {
            FlattenItem::Copy { key, asserts, .. } => (super::as_str(&key[0]), *asserts),
            other => panic!("expected a copy, got {other:?}"),
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![("added".to_string(), true), ("gone".to_string(), false)],
        "the branch added one record and removed another"
    );
    Ok(())
}
