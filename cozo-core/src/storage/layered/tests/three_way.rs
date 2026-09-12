/*
 * A three way merge, assembled from what this engine provides.
 *
 * Two branches that forked from a common base and never saw each other cannot be merged by
 * `flatten` alone: it is a two way operation, and without the base it cannot tell a branch
 * undoing its own deletion from two branches disagreeing about whether a record should exist.
 * The base is what makes those distinguishable, and supplying it is the caller's job.
 *
 * This test is that caller, in miniature. It scans each branch against the fork point, joins
 * the two streams, and adjudicates. What it demonstrates is that the engine hands up enough to
 * do so: which side touched which key, whether the touch added or removed, and what value it
 * claimed. Neither scan reports a conflict on its own, which is the point -- every conflict
 * here is a property of the pair, visible only once both are in hand.
 */

use std::collections::BTreeMap;

use miette::Result;

use super::{base, Fixture};
use crate::storage::layered::flatten::{FlattenCursor, FlattenItem};
use crate::storage::layered::{LayerRef, Stack};

/// What one branch did to one key, relative to the fork point.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Side {
    /// The branch asserts this value where the base had nothing, or had it retracted.
    Added(String),
    /// The branch retracts a record the base holds live.
    Removed,
    /// The branch wrote the key but the base already agrees: an intent to keep it, with no
    /// net change. This is what makes a delete on the other side a conflict rather than a
    /// clean apply.
    Kept,
}

/// The verdict for one key once both branches have been consulted.
#[derive(Debug, Eq, PartialEq)]
enum Verdict {
    Add(String),
    Remove,
    Conflict(&'static str),
}

/// Everything one branch changed relative to `at_fork`, read a page at a time.
fn changes_against_base(f: &Fixture, branch: &str, at_fork: &Stack) -> Result<Vec<(String, Side)>> {
    let view: Stack = vec![LayerRef::new(branch)];
    let mut out = vec![];
    let mut cursor: Option<FlattenCursor> = None;
    loop {
        let page = f.db.flatten_page(&view, at_fork, cursor.as_ref(), 2)?;
        for item in page.items {
            match item {
                FlattenItem::Copy {
                    key,
                    value,
                    asserts,
                    ..
                } => {
                    let id = super::as_str(&key[0]);
                    out.push((
                        id,
                        if asserts {
                            Side::Added(super::as_str(&value[0]))
                        } else {
                            Side::Removed
                        },
                    ));
                }
                FlattenItem::Dedupe { key, .. } => {
                    out.push((super::as_str(&key[0]), Side::Kept));
                }
                // A retraction of something the base never held: the branch intended nothing
                // the base does not already reflect, so it carries no weight in the merge.
                FlattenItem::TombstoneDropped { .. } => {}
                FlattenItem::Conflict(c) => {
                    panic!("a single branch against its own fork point should not conflict: {c:?}")
                }
            }
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(out)
}

/// Join two branches' changes and decide each key. Both inputs arrive in key order, so this
/// walks them together rather than holding either whole.
fn adjudicate(a: &[(String, Side)], b: &[(String, Side)]) -> BTreeMap<String, Verdict> {
    let mut out = BTreeMap::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        let (id, left, right) = match (a.get(i), b.get(j)) {
            (Some((ka, va)), Some((kb, vb))) if ka == kb => {
                i += 1;
                j += 1;
                (ka.clone(), Some(va), Some(vb))
            }
            (Some((ka, va)), Some((kb, _))) if ka < kb => {
                i += 1;
                (ka.clone(), Some(va), None)
            }
            (Some(_), Some((kb, vb))) => {
                j += 1;
                (kb.clone(), None, Some(vb))
            }
            (Some((ka, va)), None) => {
                i += 1;
                (ka.clone(), Some(va), None)
            }
            (None, Some((kb, vb))) => {
                j += 1;
                (kb.clone(), None, Some(vb))
            }
            (None, None) => unreachable!(),
        };
        let verdict = match (left, right) {
            // Only one side touched it, so that side decides.
            (Some(Side::Added(v)), None) | (None, Some(Side::Added(v))) => {
                Some(Verdict::Add(v.clone()))
            }
            (Some(Side::Removed), None) | (None, Some(Side::Removed)) => Some(Verdict::Remove),
            (Some(Side::Kept), None) | (None, Some(Side::Kept)) => None,
            // Both sides did the same thing.
            (Some(Side::Added(x)), Some(Side::Added(y))) if x == y => {
                Some(Verdict::Add(x.clone()))
            }
            (Some(Side::Removed), Some(Side::Removed)) => Some(Verdict::Remove),
            (Some(Side::Kept), Some(Side::Kept)) => None,
            // One kept or added it, the other took it away.
            (Some(Side::Removed), Some(_)) | (Some(_), Some(Side::Removed)) => {
                Some(Verdict::Conflict("removed on one side, kept on the other"))
            }
            // Both added, with different values.
            (Some(Side::Added(_)), Some(Side::Added(_))) => {
                Some(Verdict::Conflict("two values claimed for one key"))
            }
            // Both branches are measured against the same base, so one of them cannot see the
            // key as already present while the other sees it as new.
            (Some(Side::Added(_)), Some(Side::Kept)) | (Some(Side::Kept), Some(Side::Added(_))) => {
                unreachable!("one base cannot both hold and lack a key")
            }
            (None, None) => unreachable!(),
        };
        if let Some(verdict) = verdict {
            out.insert(id, verdict);
        }
    }
    out
}

/// The whole shape, end to end: two siblings, a common base, and a verdict for every key that
/// either of them touched.
#[test]
fn two_siblings_merge_through_their_common_base() -> Result<()> {
    let f = Fixture::new()?;

    // The common base, before either branch exists.
    f.assert_rec(&base(), "untouched", "v")?;
    f.assert_rec(&base(), "dropped-by-a", "v")?;
    f.assert_rec(&base(), "dropped-by-both", "v")?;
    f.assert_rec(&base(), "contested", "v")?;
    let fork = f.seq();
    let at_fork: Stack = vec![LayerRef::bounded("default", fork)];

    f.db.create_layer("a")?;
    f.db.create_layer("b")?;
    let a: Stack = vec![LayerRef::new("a"), LayerRef::bounded("default", fork)];
    let b: Stack = vec![LayerRef::new("b"), LayerRef::bounded("default", fork)];

    // Branch a.
    f.retract_rec(&a, "dropped-by-a")?;
    f.retract_rec(&a, "dropped-by-both")?;
    f.retract_rec(&a, "contested")?;
    f.assert_rec(&a, "only-on-a", "va")?;
    f.assert_rec(&a, "agreed", "same")?;
    f.assert_rec(&a, "disputed", "from-a")?;
    // Created and retracted inside the branch: the base never saw it, so it nets to nothing.
    f.assert_rec(&a, "ephemeral", "x")?;
    f.retract_rec(&a, "ephemeral")?;

    // Branch b, which cannot see any of that.
    f.retract_rec(&b, "dropped-by-both")?;
    // Re-affirms the record a deleted. Identical value, so no net change against the base.
    f.assert_rec(&b, "contested", "v")?;
    f.assert_rec(&b, "only-on-b", "vb")?;
    f.assert_rec(&b, "agreed", "same")?;
    f.assert_rec(&b, "disputed", "from-b")?;

    // Each branch against the fork point. Neither reports a conflict on its own.
    let from_a = changes_against_base(&f, "a", &at_fork)?;
    let from_b = changes_against_base(&f, "b", &at_fork)?;

    assert_eq!(
        from_a,
        vec![
            ("agreed".to_string(), Side::Added("same".to_string())),
            ("contested".to_string(), Side::Removed),
            ("disputed".to_string(), Side::Added("from-a".to_string())),
            ("dropped-by-a".to_string(), Side::Removed),
            ("dropped-by-both".to_string(), Side::Removed),
            ("only-on-a".to_string(), Side::Added("va".to_string())),
        ],
        "branch a's changes, with `ephemeral` netting to nothing"
    );
    assert_eq!(
        from_b,
        vec![
            ("agreed".to_string(), Side::Added("same".to_string())),
            ("contested".to_string(), Side::Kept),
            ("disputed".to_string(), Side::Added("from-b".to_string())),
            ("dropped-by-both".to_string(), Side::Removed),
            ("only-on-b".to_string(), Side::Added("vb".to_string())),
        ],
        "branch b's changes, with `contested` a keep rather than a change"
    );

    let merged = adjudicate(&from_a, &from_b);
    let expect: BTreeMap<String, Verdict> = [
        ("agreed", Verdict::Add("same".to_string())),
        (
            "contested",
            Verdict::Conflict("removed on one side, kept on the other"),
        ),
        (
            "disputed",
            Verdict::Conflict("two values claimed for one key"),
        ),
        ("dropped-by-a", Verdict::Remove),
        ("dropped-by-both", Verdict::Remove),
        ("only-on-a", Verdict::Add("va".to_string())),
        ("only-on-b", Verdict::Add("vb".to_string())),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(merged, expect);

    // `untouched` and `ephemeral` appear nowhere: neither branch changed anything about them.
    assert!(!merged.contains_key("untouched"));
    assert!(!merged.contains_key("ephemeral"));

    // The delete-and-keep disagreement is invisible to either branch on its own. That is why
    // it has to be settled here and not inside a flatten.
    assert!(f.db.flatten_plan(&vec![LayerRef::new("a")], &at_fork)?.is_clean());
    assert!(f.db.flatten_plan(&vec![LayerRef::new("b")], &at_fork)?.is_clean());
    Ok(())
}
