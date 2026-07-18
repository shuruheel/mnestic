/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `import_from_backup` must refuse a schema-mismatched restore (0.14.0
//! Part III §0 — the prerequisite that makes the `!=` type gate sound).
//!
//! The path raw-puts the source's KV rows into the destination after a key
//! rewrite — **no `coerce`, no per-value type check** — so it was the one
//! user-reachable way to smuggle a value that violates its column's declared
//! type: back up `r {k => v: Float}`, import into `r {k => v: Int}`, and a
//! `Float(1.0)` sits at rest in a declared-`Int` column. Every *query-path*
//! write coerces (`NullableColType::coerce`), so the smuggled row breaks the
//! invariant the factorized-count type gate rests on: under the engine's
//! total order `Int(1)` and `Float(1.0)` are DISTINCT (they never join), while
//! `op_neq` compares them numerically (`1 != 1.0` is false) — a divergence
//! inclusion-exclusion cannot survive.
//!
//! Discrimination: this suite was run RED against the unguarded path before
//! the `ensure!` landed (the exploit test's import returned `Ok`).
//!
//! Sqlite backend: `backup_db`/`import_from_backup` are defined in terms of a
//! SQLite backup file.

use std::collections::BTreeMap;

use cozo::{DbInstance, NamedRows, ScriptMutability};

fn new_db(path: &std::path::Path) -> DbInstance {
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{script}"))
}

/// The exploit, as a regression test: a cross-schema restore must refuse
/// instead of silently planting a Float in a declared-Int column.
#[test]
fn import_refuses_mismatched_value_types() {
    let dir = tempfile::tempdir().unwrap();
    let backup = dir.path().join("backup.db");

    let src = new_db(&dir.path().join("src.db"));
    run(&src, ":create r {k: Int => v: Float}");
    run(&src, "?[k, v] <- [[1, 1.0]] :put r {k => v}");
    src.backup_db(backup.to_str().unwrap()).unwrap();

    let dst = new_db(&dir.path().join("dst.db"));
    run(&dst, ":create r {k: Int => v: Int}");
    let res = dst.import_from_backup(backup.to_str().unwrap(), &["r".to_string()]);
    let err = format!(
        "{:?}",
        res.expect_err("a cross-schema restore must refuse, not smuggle a Float into Int")
    );
    assert!(err.contains("schema"), "error should name the cause: {err}");
    assert!(err.contains('r'), "error should name the relation: {err}");

    // Nothing was planted.
    assert_eq!(run(&dst, "?[k, v] := *r{k, v}").rows.len(), 0);
}

/// Key-side mismatches refuse too.
#[test]
fn import_refuses_mismatched_key_types() {
    let dir = tempfile::tempdir().unwrap();
    let backup = dir.path().join("backup.db");

    let src = new_db(&dir.path().join("src.db"));
    run(&src, ":create r {k: Float => v: Int}");
    run(&src, "?[k, v] <- [[1.5, 1]] :put r {k => v}");
    src.backup_db(backup.to_str().unwrap()).unwrap();

    let dst = new_db(&dir.path().join("dst.db"));
    run(&dst, ":create r {k: Int => v: Int}");
    assert!(dst
        .import_from_backup(backup.to_str().unwrap(), &["r".to_string()])
        .is_err());
}

/// An identical schema imports exactly as before — the check must not refuse
/// the legitimate case.
#[test]
fn import_with_matching_schema_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let backup = dir.path().join("backup.db");

    let src = new_db(&dir.path().join("src.db"));
    run(&src, ":create r {k: Int => v: Int}");
    run(&src, "?[k, v] <- [[1, 10], [2, 20]] :put r {k => v}");
    src.backup_db(backup.to_str().unwrap()).unwrap();

    let dst = new_db(&dir.path().join("dst.db"));
    run(&dst, ":create r {k: Int => v: Int}");
    dst.import_from_backup(backup.to_str().unwrap(), &["r".to_string()])
        .unwrap();
    assert_eq!(run(&dst, "?[k, v] := *r{k, v}").rows.len(), 2);
}

/// The check compares full metadata — column names included — which is
/// stricter than the type-safety argument strictly needs. Deliberate: a
/// restore across renamed columns is at best ambiguous about intent, and
/// refusing loudly beats guessing. This test documents the choice.
#[test]
fn import_refuses_renamed_columns_by_design() {
    let dir = tempfile::tempdir().unwrap();
    let backup = dir.path().join("backup.db");

    let src = new_db(&dir.path().join("src.db"));
    run(&src, ":create r {k: Int => v: Int}");
    run(&src, "?[k, v] <- [[1, 10]] :put r {k => v}");
    src.backup_db(backup.to_str().unwrap()).unwrap();

    let dst = new_db(&dir.path().join("dst.db"));
    run(&dst, ":create r {k: Int => w: Int}");
    assert!(dst
        .import_from_backup(backup.to_str().unwrap(), &["r".to_string()])
        .is_err());
}
