/*
 * Re-parenting a layer onto a base it was not authored over: the "restack" family.
 *
 * A stack is composed per query, so re-parenting needs no operation: it is just a different
 * `Vec<LayerRef>`. These tests cover what that composition means when the stack is not a
 * lineage. Elsewhere every lower layer is bounded at the fork its upper layer was taken from,
 * so a lower layer can never hold a newer stamp than the layer above it, and "topmost wins"
 * and "newest stamp wins" agree on every input. The stacks here break that bounding, which is
 * what makes the two rules distinguishable.
 */

use miette::Result;

use super::{base, frontier, Fixture};
use crate::storage::layered::{LayerRef, Stack};

/// Two branches taken from the same fork, neither aware of the other, composed into one stack.
/// This is the rebase-onto-a-different-branch shape: `work-a` is read over a base that is not
/// its lineage. Both contributions resolve, and history is merged rather than collapsed.
#[test]
fn a_sibling_can_be_restacked_underneath() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "root", "v0")?;
    let fork = f.seq();

    f.db.create_layer("work-a")?;
    f.db.create_layer("work-b")?;
    let a: Stack = vec![LayerRef::new("work-a"), LayerRef::bounded("default", fork)];
    let b: Stack = vec![LayerRef::new("work-b"), LayerRef::bounded("default", fork)];

    f.assert_rec(&a, "on-a", "v1")?;
    f.assert_rec(&b, "on-b", "v2")?;
    // The same record, asserted independently on both siblings. Legal: identical value.
    f.assert_rec(&a, "shared", "same")?;
    f.assert_rec(&b, "shared", "same")?;
    let bound = f.seq();

    // Neither branch sees the other while they stand apart.
    assert_eq!(
        f.live(&a, None)?,
        frontier(&[("root", "v0"), ("on-a", "v1"), ("shared", "same")])
    );

    // Restack: `work-a` over its sibling, which it never forked from.
    let restacked: Stack = vec![
        LayerRef::new("work-a"),
        LayerRef::bounded("work-b", bound),
        LayerRef::bounded("default", fork),
    ];
    assert_eq!(
        f.live(&restacked, None)?,
        frontier(&[
            ("root", "v0"),
            ("on-a", "v1"),
            ("on-b", "v2"),
            ("shared", "same")
        ])
    );

    // Restacking merges the frontier; it does not collapse history. Both siblings' assertions
    // of `shared` are still there, at their own stamps.
    let shared: Vec<_> = f
        .versions(&restacked)?
        .into_iter()
        .filter(|v| v.id == "shared")
        .collect();
    assert_eq!(shared.len(), 2, "both authorings survive: {shared:?}");
    assert!(shared[0].seq < shared[1].seq);

    // Writes through the restacked composition land in its top layer, which is still `work-a`.
    f.assert_rec(&restacked, "after-restack", "v3")?;
    assert!(f.live(&a, None)?.contains_key("after-restack"));
    assert!(!f.live(&b, None)?.contains_key("after-restack"));
    Ok(())
}

/// A layer read over a base clipped *below* the fork it was authored over. Windows are
/// per-layer, so lowering the base's ceiling withdraws the base's later rows and leaves the
/// layer's own content untouched, even though every row in it postdates the new ceiling.
#[test]
fn restacking_below_the_fork_keeps_the_layer_intact() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "early", "v0")?;
    let earlier = f.seq();
    f.assert_rec(&base(), "late", "v1")?;
    let fork = f.seq();

    f.db.create_layer("work")?;
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    let authored = f.assert_rec(&work, "on-work", "v2")?;
    assert!(
        authored > fork,
        "the branch's own row postdates the base ceiling it is about to be given"
    );

    // Re-parent onto an older base. `late` was never in this branch's lineage under the new
    // ceiling, so it goes; `on-work` stays, because the ceiling belongs to `default` alone.
    let rebased: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", earlier)];
    assert_eq!(
        f.live(&rebased, None)?,
        frontier(&[("early", "v0"), ("on-work", "v2")])
    );

    // A query-level `at` is the one ceiling that does apply to every layer at once.
    assert_eq!(
        f.live(&rebased, Some(earlier))?,
        frontier(&[("early", "v0")])
    );
    Ok(())
}

/// The discriminating case. `upper` sits above `lower`, but `lower` is written *afterwards*,
/// so it holds the newer stamp. Precedence follows the stamp, not the stack position: the
/// merge presents each key's versions newest-first across the whole stack and the ordinary
/// single-store validity rule decides from there.
///
/// Under value immutability this is observable only through retraction, which is why the rest
/// of the suite, where lower layers are always bounded at a fork and so always older, never
/// has to choose between the two readings.
#[test]
fn precedence_follows_the_stamp_not_the_stack_position() -> Result<()> {
    let f = Fixture::new()?;
    f.db.create_layer("lower")?;
    f.db.create_layer("upper")?;
    let lower: Stack = vec![LayerRef::new("lower"), LayerRef::new("default")];
    let upper: Stack = vec![
        LayerRef::new("upper"),
        LayerRef::new("lower"),
        LayerRef::new("default"),
    ];

    // Newer retraction underneath an older assertion.
    let asserted = f.assert_rec(&upper, "under", "v1")?;
    let retracted = f.retract_rec(&lower, "under")?;
    assert!(retracted > asserted, "the lower layer holds the newer stamp");

    // Older retraction underneath a newer assertion: the mirror, and the case the rest of the
    // suite exercises.
    f.retract_rec(&lower, "over")?;
    let revived = f.assert_rec(&upper, "over", "v2")?;
    assert!(revived > retracted);

    assert_eq!(f.live(&upper, None)?, frontier(&[("over", "v2")]));
    Ok(())
}
