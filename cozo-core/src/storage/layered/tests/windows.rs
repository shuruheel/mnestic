/*
 * Window semantics.
 */

use miette::Result;

use super::{base, frontier, Fixture};
use crate::storage::layered::{LayerRef, Stack};

/// The window is `(since, bound]`: a row stamped exactly at the ceiling is visible, one
/// stamped exactly at the floor is not. The fork-point row belongs to the parent.
#[test]
fn the_window_is_half_open() -> Result<()> {
    let f = Fixture::new()?;
    let a = f.assert_rec(&base(), "a", "v1")?;
    let b = f.assert_rec(&base(), "b", "v2")?;

    // Ceiling inclusive: bounding at `a`'s own stamp keeps `a`.
    assert_eq!(
        f.live(&vec![LayerRef::bounded("default", a)], None)?,
        frontier(&[("a", "v1")])
    );
    // Floor exclusive: flooring at `a`'s own stamp drops `a`.
    assert_eq!(
        f.live(
            &vec![LayerRef::windowed("default", Some(a), Some(b))],
            None
        )?,
        frontier(&[("b", "v2")])
    );
    Ok(())
}

/// `since == bound` is a legal, empty window: the natural changeset of a fork with no work.
#[test]
fn an_empty_window_is_legal() -> Result<()> {
    let f = Fixture::new()?;
    let a = f.assert_rec(&base(), "a", "v1")?;
    let empty: Stack = vec![LayerRef::windowed("default", Some(a), Some(a))];
    assert_eq!(f.live(&empty, None)?, frontier(&[]));
    Ok(())
}

/// An inverted window is always a consumer bug, and stack construction is where errors are
/// cheap and deterministic.
#[test]
fn an_inverted_window_is_rejected_at_stack_construction() -> Result<()> {
    let f = Fixture::new()?;
    let a = f.assert_rec(&base(), "a", "v1")?;
    let inverted: Stack = vec![LayerRef::windowed("default", Some(a + 10), Some(a))];
    let err = f.live(&inverted, None).unwrap_err();
    assert!(
        format!("{err}").contains("inverted window"),
        "unexpected error: {err}"
    );
    Ok(())
}

/// A query's `at` is a ceiling only. It never lowers a floor, so a historical read of a
/// changeset view yields nothing rather than the pre-window state.
#[test]
fn a_query_bound_never_lowers_a_floor() -> Result<()> {
    let f = Fixture::new()?;
    let a = f.assert_rec(&base(), "a", "v1")?;
    f.assert_rec(&base(), "b", "v2")?;

    let changeset: Stack = vec![LayerRef::windowed("default", Some(a), None)];
    assert_eq!(f.live(&changeset, Some(a))?, frontier(&[]));
    Ok(())
}

/// A floor hides the frontier: a floored layer contributes what changed inside the window,
/// not the state the window inherited.
#[test]
fn a_floor_hides_the_frontier() -> Result<()> {
    let f = Fixture::new()?;
    let established = f.assert_rec(&base(), "established", "v1")?;
    f.assert_rec(&base(), "fresh", "v2")?;

    let changeset: Stack = vec![LayerRef::windowed("default", Some(established), None)];
    // Not `established` at its old value: no `established` at all.
    assert_eq!(f.live(&changeset, None)?, frontier(&[("fresh", "v2")]));
    Ok(())
}

/// A query bound preceding the top layer's first row degenerates to a historical read of
/// the layers below it. This falls out of `min` rather than needing a branch in the code, but
/// it is the case a consumer will hit first, so it is pinned.
#[test]
fn a_query_below_the_top_layer_reads_through_it() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "root", "v1")?;
    let fork = f.seq();

    f.db.create_layer("work")?;
    let branch: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    f.assert_rec(&branch, "scratch", "v2")?;

    assert_eq!(f.live(&branch, Some(fork))?, frontier(&[("root", "v1")]));
    Ok(())
}

/// Flooring one layer of a three-layer stack affects that layer's contribution and nothing
/// else.
#[test]
fn a_mid_stack_window_is_local_to_its_layer() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "bottom", "v1")?;
    let fork1 = f.seq();

    f.db.create_layer("mid")?;
    let mid: Stack = vec![LayerRef::new("mid"), LayerRef::bounded("default", fork1)];
    let early = f.assert_rec(&mid, "mid-early", "v2")?;
    f.assert_rec(&mid, "mid-late", "v3")?;
    let fork2 = f.seq();

    f.db.create_layer("top")?;
    let full: Stack = vec![
        LayerRef::new("top"),
        LayerRef::bounded("mid", fork2),
        LayerRef::bounded("default", fork1),
    ];
    f.assert_rec(&full, "top", "v4")?;

    let floored: Stack = vec![
        LayerRef::new("top"),
        LayerRef::windowed("mid", Some(early), Some(fork2)),
        LayerRef::bounded("default", fork1),
    ];
    assert_eq!(
        f.live(&floored, None)?,
        frontier(&[("bottom", "v1"), ("mid-late", "v3"), ("top", "v4")])
    );
    Ok(())
}
