/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Both bulk-load paths bypass the per-row `:put` path that maintains HNSW / FTS
//! / LSH indexes, so rows they load are **invisible** to vector/text/LSH search
//! until the index is rebuilt. That is a design constraint, not a bug — bulk
//! loading is the fast path precisely because it skips per-row index
//! maintenance. **The bug was doing it silently.**
//!
//! `import_relations` has warned about this since the fork's early hardening
//! pass. `import_from_backup` never did (fixed in 0.12.1): it guards only against
//! *B-tree* indexes, then raw-puts the source's KV rows straight into the store —
//! so an operator restored a backup and hybrid retrieval quietly returned nothing
//! for the restored rows, with no signal anywhere.
//!
//! These tests assert the signal exists, and that it names the cure (`::reindex`).
//! The repair itself is asserted in `reindex.rs`.
//!
//! Uses the **sqlite** backend: `backup_db` / `import_from_backup` are defined in
//! terms of a SQLite backup file, and this is the real stored path.

use std::collections::BTreeMap;
use std::sync::{Mutex, Once, OnceLock};

use cozo::{DbInstance, NamedRows, ScriptMutability};

// ---------------------------------------------------------------------------
// Log capture — the fix is "emit a signal", so the test must read the signal.
// ---------------------------------------------------------------------------

fn captured() -> &'static Mutex<Vec<String>> {
    static LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(vec![]))
}

struct CaptureLogger;

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        if record.level() <= log::Level::Warn {
            captured().lock().unwrap().push(record.args().to_string());
        }
    }
    fn flush(&self) {}
}

/// Install the capturing logger once per test binary (`set_logger` is global and
/// may only be called once).
fn init_capture() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        log::set_logger(&CaptureLogger).expect("no other logger may be installed");
        log::set_max_level(log::LevelFilter::Warn);
    });
}

/// Warnings emitted while `f` ran. Serialised on the capture buffer, so the
/// tests in this file must not run their imports concurrently — they take this
/// lock for the whole call.
fn warnings_during(f: impl FnOnce()) -> Vec<String> {
    static SERIALISE: Mutex<()> = Mutex::new(());
    let _guard = SERIALISE.lock().unwrap_or_else(|e| e.into_inner());
    init_capture();
    captured().lock().unwrap().clear();
    f();
    captured().lock().unwrap().clone()
}

// ---------------------------------------------------------------------------

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

fn new_db(path: &std::path::Path) -> DbInstance {
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

fn create_doc(db: &DbInstance) {
    run(db, r":create doc {k: String => body: String}");
}

fn create_fts(db: &DbInstance) {
    run(
        db,
        r"::fts create doc:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
}

fn fts_hit_count(db: &DbInstance, query: &str) -> usize {
    run(
        db,
        &format!(r"?[k] := ~doc:fts{{k, body | query: '{query}', k: 50}}"),
    )
    .rows
    .len()
}

/// **The bug.** Restoring a backup into a relation carrying an FTS index left the
/// rows stranded — invisible to search — and said nothing at all.
///
/// Discrimination: remove the `warn_if_indexes_stranded` call from
/// `import_from_backup` and the warning assertion goes red. (The stranding
/// assertion below stays green either way — it documents the *behaviour* the
/// warning is about, and is what `::reindex` repairs.)
#[test]
fn backup_restore_warns_when_it_strands_an_fts_index() {
    let dir = tempfile::tempdir().unwrap();
    let backup = dir.path().join("backup.db");

    // Source: rows, no index.
    let src = new_db(&dir.path().join("src.db"));
    create_doc(&src);
    run(
        &src,
        r"?[k, body] <- [['k1', 'hello world'], ['k2', 'goodbye moon']] :put doc {k => body}",
    );
    src.backup_db(backup.to_str().unwrap()).unwrap();

    // Destination: same relation, WITH an FTS index, empty.
    let dst = new_db(&dir.path().join("dst.db"));
    create_doc(&dst);
    create_fts(&dst);

    let warnings = warnings_during(|| {
        dst.import_from_backup(backup.to_str().unwrap(), &["doc".to_string()])
            .unwrap();
    });

    let warned = warnings
        .iter()
        .any(|w| w.contains("doc") && w.contains("FTS") && w.contains("::reindex"));
    assert!(
        warned,
        "backup restore stranded an FTS index with NO SIGNAL — it must warn, and \
         the warning must name `::reindex` as the cure. Captured warnings: {warnings:#?}"
    );

    // The behaviour the warning is about: the rows are really there...
    let all = run(&dst, r"?[k, body] := *doc{k, body}");
    assert_eq!(all.rows.len(), 2, "the rows should have been restored");
    // ...and really invisible to full-text search.
    assert_eq!(
        fts_hit_count(&dst, "hello"),
        0,
        "restored rows are expected to be invisible to FTS until a rebuild — if this \
         ever passes, the bulk path started maintaining the index and the warning is a lie"
    );
}

/// A relation with no HNSW/FTS/LSH index strands nothing, so it must stay quiet.
/// Guards against a warning that cries wolf on every restore.
#[test]
fn backup_restore_is_quiet_when_there_is_nothing_to_strand() {
    let dir = tempfile::tempdir().unwrap();
    let backup = dir.path().join("backup.db");

    let src = new_db(&dir.path().join("src.db"));
    create_doc(&src);
    run(
        &src,
        r"?[k, body] <- [['k1', 'hello world']] :put doc {k => body}",
    );
    src.backup_db(backup.to_str().unwrap()).unwrap();

    let dst = new_db(&dir.path().join("dst.db"));
    create_doc(&dst); // no FTS/HNSW/LSH index

    let warnings = warnings_during(|| {
        dst.import_from_backup(backup.to_str().unwrap(), &["doc".to_string()])
            .unwrap();
    });

    assert!(
        !warnings.iter().any(|w| w.contains("::reindex")),
        "nothing was stranded, so nothing should have been warned about: {warnings:#?}"
    );
}

/// The sibling path's warning must keep working — and must now also name
/// `::reindex` rather than telling the user to reconstruct the index creation
/// script by hand. This is the no-regression pin for extracting the two call
/// sites into one shared helper.
#[test]
fn bulk_import_still_warns_and_now_names_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("import.db"));
    create_doc(&db);
    create_fts(&db);

    let data = BTreeMap::from([(
        "doc".to_string(),
        NamedRows::new(
            vec!["k".to_string(), "body".to_string()],
            vec![vec![
                cozo::DataValue::from("k1"),
                cozo::DataValue::from("hello world"),
            ]],
        ),
    )]);

    let warnings = warnings_during(|| {
        db.import_relations(data).unwrap();
    });

    assert!(
        warnings
            .iter()
            .any(|w| w.contains("doc") && w.contains("FTS") && w.contains("::reindex")),
        "bulk import must still warn, and must point at `::reindex`: {warnings:#?}"
    );
}
