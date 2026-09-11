/*
 * Sequence and commit stamping.
 */

use std::collections::BTreeMap;

use miette::Result;
use tempfile::TempDir;

use super::{base, Fixture};
use crate::data::value::DataValue;
use crate::runtime::db::ScriptMutability;
use crate::storage::layered::new_cozo_layered;

/// Sequential commits carry strictly increasing stamps.
#[test]
fn stamps_increase_with_commit_order() -> Result<()> {
    let f = Fixture::new()?;
    let mut stamps = vec![];
    for i in 0..5 {
        stamps.push(f.assert_rec(&base(), &format!("k{i}"), "v")?);
    }
    for pair in stamps.windows(2) {
        assert!(
            pair[0] < pair[1],
            "stamps did not increase: {:?}",
            stamps
        );
    }
    Ok(())
}

/// Every row of one transaction carries the identical stamp: a transaction is a single
/// point in commit order.
#[test]
fn one_transaction_is_one_stamp() -> Result<()> {
    let f = Fixture::new()?;
    f.db.run_on_stack(
        "?[id, at, val] <- [['a', 'ASSERT', 'v'], ['b', 'ASSERT', 'v'], ['c', 'ASSERT', 'v']]
         :put rec {id, at => val}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Mutable,
    )?;
    let stamps: Vec<_> = f.versions(&base())?.into_iter().map(|v| v.seq).collect();
    assert_eq!(stamps.len(), 3);
    assert!(
        stamps.windows(2).all(|w| w[0] == w[1]),
        "rows of one transaction were stamped apart: {:?}",
        stamps
    );
    Ok(())
}

/// A transaction is indivisible under time travel: a read at its stamp sees all of its
/// rows, and a read at any earlier captured sequence sees none of them.
#[test]
fn a_transaction_is_atomic_under_time_travel() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "old", "v0")?;
    let before = f.seq();

    f.db.run_on_stack(
        "?[id, at, val] <- [['a', 'ASSERT', 'v'], ['b', 'ASSERT', 'v']]
         :put rec {id, at => val}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Mutable,
    )?;
    let stamp = f.versions(&base())?.iter().map(|v| v.seq).max().unwrap();

    assert_eq!(f.live(&base(), Some(before))?.len(), 1);
    assert_eq!(f.live(&base(), Some(stamp))?.len(), 3);
    Ok(())
}

/// A fork point never subsequently acquires rows.
///
/// Pinned so that stamping cannot drift back to write time: were the sequence read when the
/// row was buffered rather than when it was committed, a fork captured in between would grow a
/// row underneath it.
#[test]
fn a_fork_point_is_stable() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "before-fork", "v1")?;
    let fork = f.seq();

    for i in 0..5 {
        f.assert_rec(&base(), &format!("after{i}"), "v")?;
    }

    for v in f.versions(&base())? {
        if v.id != "before-fork" {
            assert!(
                v.seq > fork,
                "a row committed after the fork was stamped at or below it: {:?} vs {}",
                v,
                fork
            );
        }
    }
    assert_eq!(f.live(&base(), Some(fork))?.len(), 1);
    Ok(())
}

/// Sparseness tolerance: unrelated writes move the counter, and nothing may depend on
/// consecutiveness. This is the meta-test for the no-hardcoded-sequences convention.
#[test]
fn expectations_survive_unrelated_writes() -> Result<()> {
    let f = Fixture::new()?;
    let first = f.assert_rec(&base(), "k", "v1")?;

    // An unrelated relation churns the sequence counter between the writes under test.
    f.db.run_script(
        ":create noise {k: Int => v: Int}",
        Default::default(),
        ScriptMutability::Mutable,
    )?;
    for i in 0..20 {
        let mut params = BTreeMap::new();
        params.insert("k".to_string(), DataValue::from(i));
        f.db.run_script(
            "?[k, v] <- [[$k, $k]] :put noise {k => v}",
            params,
            ScriptMutability::Mutable,
        )?;
    }

    let second = f.assert_rec(&base(), "k2", "v2")?;
    assert!(second > first);
    assert_eq!(f.live(&base(), Some(first))?.len(), 1);
    assert_eq!(f.live(&base(), Some(second))?.len(), 2);
    Ok(())
}

/// Restart continuity: persisted stamps are unchanged by a reopen, and new stamps land
/// strictly above every persisted one.
#[test]
fn stamps_survive_a_restart() -> Result<()> {
    let dir = TempDir::new().unwrap();
    let persisted = {
        let db = new_cozo_layered(dir.path())?;
        db.run_script(
            ":create rec {id: String, at: Validity => val: String}",
            Default::default(),
            ScriptMutability::Mutable,
        )?;
        db.run_on_stack(
            "?[id, at, val] <- [['a', 'ASSERT', 'v']] :put rec {id, at => val}",
            Default::default(),
            &base(),
            None,
            ScriptMutability::Mutable,
        )?;
        read_stamps(&db)?
    };
    assert_eq!(persisted.len(), 1);

    let db = new_cozo_layered(dir.path())?;
    assert_eq!(read_stamps(&db)?, persisted);
    assert!(db.current_seq()? >= persisted[0]);

    db.run_on_stack(
        "?[id, at, val] <- [['b', 'ASSERT', 'v']] :put rec {id, at => val}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Mutable,
    )?;
    let after = read_stamps(&db)?;
    assert!(
        after.iter().all(|s| *s >= persisted[0]),
        "a post-restart stamp landed below a persisted one"
    );
    assert!(after.iter().any(|s| *s > persisted[0]));
    Ok(())
}

/// Explicit validity is rejected. A user-supplied sequence above the current one would
/// shadow future writes and break layer isolation; one below it would appear to predate a fork
/// it postdates. Failing beats accepting it, and beats silently ignoring it.
#[test]
fn explicit_validity_is_rejected() -> Result<()> {
    let f = Fixture::new()?;

    for literal in ["[12345, true]", "'2023-01-01T00:00:00Z'", "'~2023-01-01T00:00:00Z'"] {
        let script = format!(
            "?[id, at, val] <- [['k', {literal}, 'v']] :put rec {{id, at => val}}"
        );
        let err = f
            .db
            .run_on_stack(
                &script,
                Default::default(),
                &base(),
                None,
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("explicit validity"),
            "{literal} was accepted or failed for the wrong reason: {err:?}"
        );
    }

    // The accepted inputs still work, through the ordinary entry point as well as `run_on_stack`.
    f.assert_rec(&base(), "k", "v")?;
    f.db.run_script(
        "?[id, at, val] <- [['k2', 'ASSERT', 'v']] :put rec {id, at => val}",
        Default::default(),
        ScriptMutability::Mutable,
    )?;
    assert_eq!(f.live(&base(), None)?.len(), 2);
    // Rows written through the ordinary entry point are stamped in sequence, not wall clock.
    let ceiling = f.seq();
    assert!(f.versions(&base())?.iter().all(|v| v.seq <= ceiling));
    Ok(())
}

fn read_stamps(db: &crate::Db<crate::storage::layered::LayeredStorage>) -> Result<Vec<i64>> {
    let rows = db.run_on_stack(
        "?[id, at] := *rec{id, at}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Immutable,
    )?;
    let mut ret: Vec<i64> = rows
        .rows
        .iter()
        .map(|row| match &row[1] {
            DataValue::Validity(v) => v.timestamp.0 .0,
            other => panic!("expected a validity, got {:?}", other),
        })
        .collect();
    ret.sort();
    Ok(ret)
}
