/*
 * Concurrency.
 *
 * Concurrent access to different stacks is the normal case for any consumer that needs layers
 * at all, so these run real threads rather than interleaving by hand.
 */

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use miette::Result;

use super::{base, frontier, Fixture};
use crate::storage::layered::{LayerRef, Stack};

/// Writes hammered through one stack never change what another stack reads, as long as
/// they do not share a top layer. This is what "writes go to the top layer only" buys.
#[test]
fn writes_through_one_stack_do_not_disturb_another() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "seed", "v0")?;
    let fork = f.seq();
    f.db.create_layer("writer")?;
    f.db.create_layer("reader")?;

    let writer: Stack = vec![LayerRef::new("writer"), LayerRef::bounded("default", fork)];
    let reader: Stack = vec![LayerRef::new("reader"), LayerRef::bounded("default", fork)];
    let expected = f.live(&reader, None)?;

    let stop = AtomicBool::new(false);
    thread::scope(|scope| -> Result<()> {
        let handle = scope.spawn(|| -> Result<()> {
            for i in 0..40 {
                f.assert_rec(&writer, &format!("w{i}"), "v")?;
            }
            stop.store(true, Ordering::Release);
            Ok(())
        });

        while !stop.load(Ordering::Acquire) {
            assert_eq!(
                f.live(&reader, None)?,
                expected,
                "a concurrent write through another stack changed this one"
            );
        }
        handle.join().unwrap()
    })?;

    assert_eq!(f.live(&reader, None)?, expected);
    assert_eq!(f.live(&writer, None)?.len(), 41);
    Ok(())
}

/// A branch bounded at a fork point reads the same thing for its whole life, however busy
/// its parent is. The fork point is the entire mechanism: no snapshot, no copy.
#[test]
fn a_bounded_branch_is_repeatable_under_a_busy_parent() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "at-fork", "v0")?;
    let fork = f.seq();
    f.db.create_layer("branch")?;
    let branch: Stack = vec![LayerRef::new("branch"), LayerRef::bounded("default", fork)];
    let expected = frontier(&[("at-fork", "v0")]);

    let stop = AtomicBool::new(false);
    thread::scope(|scope| -> Result<()> {
        let handle = scope.spawn(|| -> Result<()> {
            for i in 0..40 {
                f.assert_rec(&base(), &format!("parent{i}"), "v")?;
            }
            // The parent also deletes the record the branch is reading.
            f.retract_rec(&base(), "at-fork")?;
            stop.store(true, Ordering::Release);
            Ok(())
        });

        while !stop.load(Ordering::Acquire) {
            assert_eq!(f.live(&branch, None)?, expected, "the fork point moved");
        }
        handle.join().unwrap()
    })?;

    assert_eq!(f.live(&branch, None)?, expected);
    Ok(())
}

/// Two heads committing concurrently over one base: each head's changeset holds exactly
/// its own rows and never its sibling's.
#[test]
fn concurrent_heads_have_pure_changesets() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "shared", "v0")?;
    let fork = f.seq();
    f.db.create_layer("head-a")?;
    f.db.create_layer("head-b")?;

    let a: Stack = vec![LayerRef::new("head-a"), LayerRef::bounded("default", fork)];
    let b: Stack = vec![LayerRef::new("head-b"), LayerRef::bounded("default", fork)];

    thread::scope(|scope| -> Result<()> {
        let ha = scope.spawn(|| -> Result<()> {
            for i in 0..25 {
                f.assert_rec(&a, &format!("a{i}"), "v")?;
            }
            Ok(())
        });
        let hb = scope.spawn(|| -> Result<()> {
            for i in 0..25 {
                f.assert_rec(&b, &format!("b{i}"), "v")?;
            }
            Ok(())
        });
        ha.join().unwrap()?;
        hb.join().unwrap()
    })?;

    let changes_a = f.live(&vec![LayerRef::new("head-a")], None)?;
    let changes_b = f.live(&vec![LayerRef::new("head-b")], None)?;
    assert_eq!(changes_a.len(), 25);
    assert_eq!(changes_b.len(), 25);
    assert!(changes_a.keys().all(|k| k.starts_with('a')));
    assert!(changes_b.keys().all(|k| k.starts_with('b')));
    Ok(())
}

/// Two transactions committing concurrently never share a stamp, and stamp order is commit
/// order.
#[test]
fn concurrent_commits_never_share_a_stamp() -> Result<()> {
    let f = Fixture::new()?;
    f.db.create_layer("t1")?;
    f.db.create_layer("t2")?;
    let fork = f.seq();
    let s1: Stack = vec![LayerRef::new("t1"), LayerRef::bounded("default", fork)];
    let s2: Stack = vec![LayerRef::new("t2"), LayerRef::bounded("default", fork)];

    thread::scope(|scope| -> Result<()> {
        let h1 = scope.spawn(|| -> Result<()> {
            for i in 0..30 {
                f.assert_rec(&s1, &format!("x{i}"), "v")?;
            }
            Ok(())
        });
        let h2 = scope.spawn(|| -> Result<()> {
            for i in 0..30 {
                f.assert_rec(&s2, &format!("y{i}"), "v")?;
            }
            Ok(())
        });
        h1.join().unwrap()?;
        h2.join().unwrap()
    })?;

    let mut stamps: Vec<i64> = f
        .versions(&vec![LayerRef::new("t1")])?
        .into_iter()
        .chain(f.versions(&vec![LayerRef::new("t2")])?)
        .map(|v| v.seq)
        .collect();
    let total = stamps.len();
    stamps.sort_unstable();
    stamps.dedup();
    assert_eq!(stamps.len(), total, "two commits shared a stamp");
    Ok(())
}
