/*
 * Crash recovery.
 *
 * A crash test needs a real crash, so these re-invoke the test binary as a child process, let
 * it do its work, and abort it. The child bodies are `#[ignore]`d so a normal run skips them;
 * the parent runs them by name with `--ignored`.
 */

use std::path::PathBuf;
use std::process::Command;

use miette::Result;

use super::{base, frontier};
use crate::runtime::db::ScriptMutability;
use crate::storage::layered::{new_cozo_layered, LayerRef, Stack};

const DIR_VAR: &str = "COZO_LAYERED_CRASH_DIR";
const SCHEMA: &str = ":create rec {id: String, at: Validity => val: String}";

fn child_dir() -> Option<PathBuf> {
    std::env::var_os(DIR_VAR).map(PathBuf::from)
}

/// Run one of the `#[ignore]`d child tests below in a fresh process against `dir`, and return
/// whether it exited normally. A child that aborts as designed reports failure here.
fn run_child(test: &str, dir: &PathBuf) -> bool {
    let exe = std::env::current_exe().expect("the test binary must be locatable");
    Command::new(exe)
        .args(["--exact", test, "--ignored", "--nocapture", "--test-threads=1"])
        .env(DIR_VAR, dir)
        .status()
        .expect("failed to spawn the child process")
        .success()
}

/// Killed after a commit, the rows and their stamps survive recovery intact.
#[test]
fn a_commit_survives_a_kill() -> Result<()> {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    let survived = run_child("storage::layered::tests::crash::child_commits_then_aborts", &path);
    assert!(!survived, "the child was supposed to abort");

    let db = new_cozo_layered(&path)?;
    let rows = db.run_on_stack(
        "?[id, val] := *rec{id, val @ 'NOW'}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Immutable,
    )?;
    assert_eq!(rows.rows.len(), 3, "committed rows did not survive the kill");

    // Recovery must not rewind the sequence: new writes still land above everything persisted.
    let after = db.current_seq()?;
    db.run_on_stack(
        "?[id, at, val] <- [['post', 'ASSERT', 'v']] :put rec {id, at => val}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Mutable,
    )?;
    let stamps = db.run_on_stack(
        "?[id, at] := *rec{id, at}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Immutable,
    )?;
    let newest = stamps
        .rows
        .iter()
        .map(|r| match &r[1] {
            crate::DataValue::Validity(v) => v.timestamp.0 .0,
            other => panic!("expected a validity, got {other:?}"),
        })
        .max()
        .unwrap();
    assert!(newest > after, "a post-recovery stamp did not advance");
    Ok(())
}

#[test]
#[ignore = "child process of a_commit_survives_a_kill"]
fn child_commits_then_aborts() {
    let Some(dir) = child_dir() else { return };
    let db = new_cozo_layered(&dir).unwrap();
    db.run_script(SCHEMA, Default::default(), ScriptMutability::Mutable)
        .unwrap();
    db.run_on_stack(
        "?[id, at, val] <- [['a', 'ASSERT', 'v'], ['b', 'ASSERT', 'v'], ['c', 'ASSERT', 'v']]
         :put rec {id, at => val}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Mutable,
    )
    .unwrap();
    // The commit returned, so the write-ahead log has it. Die without unwinding.
    std::process::abort();
}

/// Killed during a flatten, rerunning the identical flatten converges on the same
/// destination a clean single run produces. Idempotence is the recovery mechanism, and this is
/// it under an actual kill.
///
/// A flatten is one transaction here, so the crash finds it either wholly applied or not at
/// all; either way the rerun is what makes the outcome definite.
#[test]
fn a_flatten_recovers_by_rerunning() -> Result<()> {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    let survived = run_child(
        "storage::layered::tests::crash::child_flattens_then_aborts",
        &path,
    );
    assert!(!survived, "the child was supposed to abort");

    let db = new_cozo_layered(&path)?;
    let view: Stack = vec![LayerRef::new("work")];
    // Recovery: run the same flatten again. It must succeed whether or not the first landed.
    db.flatten(&view, &base(), true)?;
    let recovered = read_frontier(&db)?;

    // What a clean, uninterrupted run produces, in a store built the same way.
    let clean_dir = tempfile::TempDir::new().unwrap();
    let clean = new_cozo_layered(clean_dir.path())?;
    clean.run_script(SCHEMA, Default::default(), ScriptMutability::Mutable)?;
    seed(&clean)?;
    clean.flatten(&view, &base(), true)?;
    assert_eq!(recovered, read_frontier(&clean)?);

    // And it is the frontier the work called for.
    assert_eq!(
        recovered,
        frontier(&[("kept", "v0"), ("added0", "v"), ("added1", "v"), ("added2", "v")])
    );
    Ok(())
}

#[test]
#[ignore = "child process of a_flatten_recovers_by_rerunning"]
fn child_flattens_then_aborts() {
    let Some(dir) = child_dir() else { return };
    let db = new_cozo_layered(&dir).unwrap();
    db.run_script(SCHEMA, Default::default(), ScriptMutability::Mutable)
        .unwrap();
    seed(&db).unwrap();

    let view: Stack = vec![LayerRef::new("work")];
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _ = db.flatten(&view, &base(), true);
        });
        // Racy on purpose: the abort lands somewhere inside the flatten, and the assertion in
        // the parent holds wherever that is.
        std::thread::sleep(std::time::Duration::from_micros(300));
        std::process::abort();
    });
}

fn seed(db: &crate::Db<crate::storage::layered::LayeredStorage>) -> Result<()> {
    db.run_on_stack(
        "?[id, at, val] <- [['kept', 'ASSERT', 'v0'], ['doomed', 'ASSERT', 'v1']]
         :put rec {id, at => val}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Mutable,
    )?;
    db.create_layer("work")?;
    let fork = db.current_seq()?;
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    for i in 0..3 {
        db.run_on_stack(
            &format!("?[id, at, val] <- [['added{i}', 'ASSERT', 'v']] :put rec {{id, at => val}}"),
            Default::default(),
            &work,
            None,
            ScriptMutability::Mutable,
        )?;
    }
    db.run_on_stack(
        "?[id, at, val] <- [['doomed', 'RETRACT', '']] :put rec {id, at => val}",
        Default::default(),
        &work,
        None,
        ScriptMutability::Mutable,
    )?;
    Ok(())
}

fn read_frontier(
    db: &crate::Db<crate::storage::layered::LayeredStorage>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let rows = db.run_on_stack(
        "?[id, val] := *rec{id, val @ 'NOW'}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Immutable,
    )?;
    Ok(rows
        .rows
        .into_iter()
        .map(|r| (super::as_str(&r[0]), super::as_str(&r[1])))
        .collect())
}
