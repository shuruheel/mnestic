/*
 * Layer lifecycle and catalog.
 */

use miette::Result;
use tempfile::TempDir;

use super::{base, frontier, Fixture};
use crate::runtime::db::ScriptMutability;
use crate::storage::layered::{new_cozo_layered, LayerId, LayerRef, Stack};

/// Create, list, write, drop.
#[test]
fn a_layer_round_trips() -> Result<()> {
    let f = Fixture::new()?;
    assert_eq!(f.db.list_layers()?, vec![LayerId("default".into())]);

    f.db.create_layer("work")?;
    assert_eq!(
        f.db.list_layers()?,
        vec![LayerId("default".into()), LayerId("work".into())]
    );

    let fork = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    f.assert_rec(&work, "k", "v1")?;

    f.db.drop_layer("work")?;
    assert_eq!(f.db.list_layers()?, vec![LayerId("default".into())]);
    Ok(())
}

/// A recreated layer is empty. Nothing the dropped one held comes back.
#[test]
fn a_dropped_layer_does_not_resurrect() -> Result<()> {
    let f = Fixture::new()?;
    f.db.create_layer("work")?;
    let fork = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
    f.assert_rec(&work, "scratch", "v1")?;
    assert_eq!(f.live(&work, None)?, frontier(&[("scratch", "v1")]));

    f.db.drop_layer("work")?;
    f.db.create_layer("work")?;
    assert_eq!(f.live(&work, None)?, frontier(&[]));
    Ok(())
}

/// Layers and their contents survive a reopen, and stacks rebuild from consumer-held
/// metadata: the fork point is just a number the consumer wrote down.
#[test]
fn layers_persist_across_a_reopen() -> Result<()> {
    let dir = TempDir::new().unwrap();
    let fork = {
        let db = new_cozo_layered(dir.path())?;
        db.run_script(
            ":create rec {id: String, at: Validity => val: String}",
            Default::default(),
            ScriptMutability::Mutable,
        )?;
        db.run_on_stack(
            "?[id, at, val] <- [['root', 'ASSERT', 'v1']] :put rec {id, at => val}",
            Default::default(),
            &base(),
            None,
            ScriptMutability::Mutable,
        )?;
        db.create_layer("work")?;
        let fork = db.current_seq()?;
        db.run_on_stack(
            "?[id, at, val] <- [['scratch', 'ASSERT', 'v2']] :put rec {id, at => val}",
            Default::default(),
            &vec![LayerRef::new("work"), LayerRef::bounded("default", fork)],
            None,
            ScriptMutability::Mutable,
        )?;
        fork
    };

    let db = new_cozo_layered(dir.path())?;
    assert_eq!(
        db.list_layers()?,
        vec![LayerId("default".into()), LayerId("work".into())]
    );
    let rows = db.run_on_stack(
        "?[id, val] := *rec{id, val @ 'NOW'}",
        Default::default(),
        &vec![LayerRef::new("work"), LayerRef::bounded("default", fork)],
        None,
        ScriptMutability::Immutable,
    )?;
    assert_eq!(rows.rows.len(), 2);
    Ok(())
}

/// A stack naming a layer that is gone fails at construction, not mid-query.
#[test]
fn a_missing_layer_fails_at_stack_construction() -> Result<()> {
    let f = Fixture::new()?;
    let err = f
        .live(&vec![LayerRef::new("never-created")], None)
        .unwrap_err();
    assert!(format!("{err}").contains("no such layer"), "{err}");

    f.db.create_layer("work")?;
    f.db.drop_layer("work")?;
    let err = f.live(&vec![LayerRef::new("work")], None).unwrap_err();
    assert!(format!("{err}").contains("no such layer"), "{err}");
    Ok(())
}

/// The default layer carries the catalog and the existing entry points, so it cannot be
/// dropped.
#[test]
fn the_default_layer_is_protected() -> Result<()> {
    let f = Fixture::new()?;
    let err = f.db.drop_layer("default").unwrap_err();
    assert!(format!("{err}").contains("cannot be dropped"), "{err}");
    Ok(())
}

/// A stack that names the same layer twice has no coherent shadowing order, and is refused.
#[test]
fn a_layer_cannot_appear_twice_in_one_stack() -> Result<()> {
    let f = Fixture::new()?;
    f.db.create_layer("work")?;
    let err = f
        .live(&vec![LayerRef::new("work"), LayerRef::new("work")], None)
        .unwrap_err();
    assert!(format!("{err}").contains("twice"), "{err}");
    Ok(())
}

/// The catalog is global: a relation created through one stack is visible through every
/// other, while its *contents* stay in the layer that wrote them.
#[test]
fn the_catalog_is_global_and_contents_are_local() -> Result<()> {
    let f = Fixture::new()?;
    f.db.create_layer("work")?;
    let fork = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];

    f.db.run_on_stack(
        ":create scratch {id: String, at: Validity => val: String}",
        Default::default(),
        &work,
        None,
        ScriptMutability::Mutable,
    )?;
    f.db.run_on_stack(
        "?[id, at, val] <- [['a', 'ASSERT', 'v']] :put scratch {id, at => val}",
        Default::default(),
        &work,
        None,
        ScriptMutability::Mutable,
    )?;

    // Definition visible from the base stack...
    let from_base = f.db.run_on_stack(
        "?[id, val] := *scratch{id, val @ 'NOW'}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Immutable,
    )?;
    // ...contents are not.
    assert!(from_base.rows.is_empty());

    let from_work = f.db.run_on_stack(
        "?[id, val] := *scratch{id, val @ 'NOW'}",
        Default::default(),
        &work,
        None,
        ScriptMutability::Immutable,
    )?;
    assert_eq!(from_work.rows.len(), 1);
    Ok(())
}

/// Stackability is settled when the relation is created (a validity column or not), and
/// enforced whenever the relation is reached through a multi-layer stack, before any row is
/// read. The same relation through a single-layer stack works normally.
#[test]
fn a_non_stackable_relation_is_refused_by_a_multi_layer_stack() -> Result<()> {
    let f = Fixture::new()?;
    f.db.run_script(
        ":create plain {k: Int => v: Int}",
        Default::default(),
        ScriptMutability::Mutable,
    )?;
    f.db.run_script(
        "?[k, v] <- [[1, 10]] :put plain {k => v}",
        Default::default(),
        ScriptMutability::Mutable,
    )?;

    f.db.create_layer("work")?;
    let fork = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];

    // Reading it through the stack fails, and says why.
    let err = f
        .db
        .run_on_stack(
            "?[k, v] := *plain{k, v}",
            Default::default(),
            &work,
            None,
            ScriptMutability::Immutable,
        )
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("no validity column"),
        "unexpected error: {err:?}"
    );

    // Writing to it through the stack fails the same way.
    let err = f
        .db
        .run_on_stack(
            "?[k, v] <- [[2, 20]] :put plain {k => v}",
            Default::default(),
            &work,
            None,
            ScriptMutability::Mutable,
        )
        .unwrap_err();
    assert!(format!("{err:?}").contains("no validity column"), "{err:?}");

    // Through a single-layer stack it is an ordinary relation.
    let rows = f.db.run_on_stack(
        "?[k, v] := *plain{k, v}",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Immutable,
    )?;
    assert_eq!(rows.rows.len(), 1);
    Ok(())
}

/// Destructive schema changes need a single-layer stack: through a multi-layer one they
/// would strike layers the caller is not writing to, since the catalog is global but the rows
/// are not.
#[test]
fn destructive_ddl_needs_a_single_layer_stack() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v1")?;
    f.db.create_layer("work")?;
    let fork = f.seq();
    let work: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];

    let err = f
        .db
        .run_on_stack(
            "::remove rec",
            Default::default(),
            &work,
            None,
            ScriptMutability::Mutable,
        )
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("single-layer stack"),
        "unexpected error: {err:?}"
    );
    // The relation is intact.
    assert_eq!(f.live(&base(), None)?, frontier(&[("k", "v1")]));

    // Additive DDL through the same stack is fine: definitions are global and benign (L7).
    f.db.run_on_stack(
        ":create added {id: String, at: Validity => val: String}",
        Default::default(),
        &work,
        None,
        ScriptMutability::Mutable,
    )?;

    // And through a single-layer stack, removal works.
    f.db.run_on_stack(
        "::remove added",
        Default::default(),
        &base(),
        None,
        ScriptMutability::Mutable,
    )?;
    Ok(())
}

/// Restoring a backup into a layered store is refused when the restored rows carry sequences
/// the target store's own counter has not reached.
///
/// The stamps in a backup come from the sequence space of the store that produced them, and a
/// fresh store's counter starts near zero. Writing to a store in that state would stamp new
/// rows *below* restored ones, inverting history and breaking every fork point taken
/// afterwards. There is no way to fast-forward RocksDB's counter, so this fails loudly instead.
#[cfg(feature = "storage-sqlite")]
#[test]
fn restoring_a_backup_with_higher_sequences_is_refused() -> Result<()> {
    let source_dir = TempDir::new().unwrap();
    let backup = TempDir::new().unwrap();
    let backup_file = backup.path().join("backup.db");

    let source = new_cozo_layered(source_dir.path())?;
    source.run_script(
        ":create rec {id: String, at: Validity => val: String}",
        Default::default(),
        ScriptMutability::Mutable,
    )?;
    for i in 0..50 {
        source.run_on_stack(
            &format!("?[id, at, val] <- [['k{i}', 'ASSERT', 'v']] :put rec {{id, at => val}}"),
            Default::default(),
            &base(),
            None,
            ScriptMutability::Mutable,
        )?;
    }
    source.backup_db(&backup_file)?;

    let target_dir = TempDir::new().unwrap();
    let target = new_cozo_layered(target_dir.path())?;
    let err = target.restore_backup(&backup_file).unwrap_err();
    assert!(
        format!("{err:?}").contains("sequences up to"),
        "unexpected error: {err:?}"
    );
    Ok(())
}

/// The existing entry points run against a single-layer stack on the default column family
/// and behave as they always did.
#[test]
fn the_existing_entry_points_still_work() -> Result<()> {
    let f = Fixture::new()?;
    f.db.run_script(
        ":create plain {k: Int => v: Int}",
        Default::default(),
        ScriptMutability::Mutable,
    )?;
    f.db.run_script(
        "?[k, v] <- [[1, 10], [2, 20]] :put plain {k => v}",
        Default::default(),
        ScriptMutability::Mutable,
    )?;
    let rows = f.db.run_script(
        "?[k, v] := *plain{k, v} :order k",
        Default::default(),
        ScriptMutability::Immutable,
    )?;
    assert_eq!(rows.rows.len(), 2);

    // A relation with no validity column has no retraction mechanism, so it is only coherent
    // in a single-layer stack; the multi-layer case is checked in the stackability suite.
    Ok(())
}

/// A value blob that will not decode is an error, not a panic.
///
/// The engine keeps every version of every record, so a single unreadable row must not make
/// the rest of the history unreachable. The other backends decode fallibly for this reason;
/// this pins the layered read path to the same contract.
#[test]
fn a_corrupt_value_is_an_error_not_a_panic() -> Result<()> {
    use crate::runtime::relation::RelationId;
    use crate::storage::{StoreTx, Storage};

    let f = Fixture::new()?;
    f.assert_rec(&base(), "k", "v")?;

    // Find the raw key of a stored record, skipping the system relation that holds the catalog.
    let lower = RelationId::SYSTEM.next().raw_encode().to_vec();
    let upper = vec![0xffu8; 8];
    let key = {
        let tx = f.db.db.transact(false)?;
        let mut it = tx.range_scan(&lower, &upper);
        it.next().expect("the record just written should be there")?.0
    };

    // Overwrite its value with bytes that are not a value blob. `batch_put` is the restore
    // path, so it writes raw pairs without going through the encoder.
    f.db.db
        .batch_put(Box::new(std::iter::once(Ok((key, vec![0xABu8; 5])))))?;

    let err = f
        .live(&base(), None)
        .expect_err("a corrupt value should surface as an error");
    // Specifically the corrupt-blob diagnostic, not merely "some error": the point is that the
    // engine reports the unreadable row rather than dying on it.
    let msg = format!("{err:?}");
    assert!(
        msg.contains("value blob") || msg.contains("CorruptValueBlob") || msg.contains("corrupt"),
        "expected a corrupt-value diagnostic, got: {msg}"
    );
    Ok(())
}

