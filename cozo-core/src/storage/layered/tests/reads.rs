/*
 * Point reads, shadowing, and the merge iterator.
 */

use miette::Result;

use super::{base, frontier, Fixture};
use crate::runtime::db::ScriptMutability;
use crate::storage::layered::{LayerRef, Stack};

/// Three layers, `top` over `mid` over `default`, all unwindowed.
///
/// Windows are exercised in their own suite; leaving them open here keeps these tests about
/// shadowing and merge order alone, and lets a write to any layer happen at any point.
fn three_layers(f: &Fixture) -> Result<(Stack, Stack, Stack)> {
    f.db.create_layer("mid")?;
    f.db.create_layer("top")?;
    let base_stack = base();
    let mid: Stack = vec![LayerRef::new("mid"), LayerRef::new("default")];
    let top: Stack = vec![
        LayerRef::new("top"),
        LayerRef::new("mid"),
        LayerRef::new("default"),
    ];
    Ok((base_stack, mid, top))
}

/// A key held in any single position of the stack resolves, with the right value.
#[test]
fn a_key_resolves_from_any_position() -> Result<()> {
    let f = Fixture::new()?;
    let (base_stack, mid, top) = three_layers(&f)?;
    f.assert_rec(&base_stack, "bottom", "v1")?;
    f.assert_rec(&mid, "middle", "v2")?;
    f.assert_rec(&top, "upper", "v3")?;

    assert_eq!(
        f.live(&top, None)?,
        frontier(&[("bottom", "v1"), ("middle", "v2"), ("upper", "v3")])
    );
    Ok(())
}

/// A key absent from every layer is absent, and no neighbouring key leaks in.
#[test]
fn an_absent_key_stays_absent() -> Result<()> {
    let f = Fixture::new()?;
    let (base_stack, mid, top) = three_layers(&f)?;
    f.assert_rec(&base_stack, "aa", "v1")?;
    f.assert_rec(&mid, "ac", "v2")?;

    let rows = f.db.run_on_stack(
        "?[val] := *rec{id: 'ab', val @ 'NOW'}",
        Default::default(),
        &top,
        None,
        ScriptMutability::Immutable,
    )?;
    assert!(rows.rows.is_empty(), "a neighbouring key leaked in");
    Ok(())
}

/// Keys interleaved across three layers emit in key order, and a key several layers
/// hold emits exactly once.
///
/// Value immutability means two layers can never legitimately disagree about a
/// key's *value*, so "the topmost layer wins" is observable as deduplication here and as
/// precedence in the retraction case below, never as one value beating another.
#[test]
fn a_scan_merges_in_key_order_and_emits_each_key_once() -> Result<()> {
    let f = Fixture::new()?;
    let (base_stack, mid, top) = three_layers(&f)?;
    f.assert_rec(&base_stack, "a", "va")?;
    f.assert_rec(&base_stack, "d", "vd")?;
    f.assert_rec(&mid, "b", "vb")?;
    f.assert_rec(&mid, "d", "vd")?;
    f.assert_rec(&top, "c", "vc")?;
    f.assert_rec(&top, "d", "vd")?;

    let rows = f.db.run_on_stack(
        "?[id, val] := *rec{id, val @ 'NOW'} :order id",
        Default::default(),
        &top,
        None,
        ScriptMutability::Immutable,
    )?;
    let seen: Vec<(String, String)> = rows
        .rows
        .iter()
        .map(|r| (super::as_str(&r[0]), super::as_str(&r[1])))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("a".to_string(), "va".to_string()),
            ("b".to_string(), "vb".to_string()),
            ("c".to_string(), "vc".to_string()),
            ("d".to_string(), "vd".to_string()),
        ]
    );
    Ok(())
}

/// The precedence half of R2: a retraction in a higher layer wins over the lower layer's row,
/// whichever layer wrote it.
#[test]
fn a_higher_layer_takes_precedence() -> Result<()> {
    let f = Fixture::new()?;
    let (base_stack, mid, top) = three_layers(&f)?;
    f.assert_rec(&base_stack, "a", "va")?;
    f.assert_rec(&base_stack, "b", "vb")?;
    f.retract_rec(&mid, "a")?;
    f.retract_rec(&top, "b")?;

    assert_eq!(f.live(&top, None)?, frontier(&[]));
    assert_eq!(f.live(&mid, None)?, frontier(&[("b", "vb")]));
    assert_eq!(
        f.live(&base_stack, None)?,
        frontier(&[("a", "va"), ("b", "vb")])
    );
    Ok(())
}

/// Empty layers mid-stack, and a stack that is empty throughout, scan cleanly.
#[test]
fn empty_layers_scan_cleanly() -> Result<()> {
    let f = Fixture::new()?;
    let (base_stack, _mid, top) = three_layers(&f)?;
    assert_eq!(f.live(&top, None)?, frontier(&[]));

    f.assert_rec(&base_stack, "only", "v1")?;
    assert_eq!(f.live(&top, None)?, frontier(&[("only", "v1")]));
    Ok(())
}

/// A scan of one relation emits no rows of another, from any layer.
#[test]
fn relations_do_not_leak_across_layers() -> Result<()> {
    let f = Fixture::new()?;
    f.db.run_script(
        ":create other {id: String, at: Validity => val: String}",
        Default::default(),
        ScriptMutability::Mutable,
    )?;
    let (base_stack, _mid, top) = three_layers(&f)?;
    f.assert_rec(&base_stack, "in-rec", "v1")?;
    f.db.run_on_stack(
        "?[id, at, val] <- [['in-other', 'ASSERT', 'v2']] :put other {id, at => val}",
        Default::default(),
        &top,
        None,
        ScriptMutability::Mutable,
    )?;

    assert_eq!(f.live(&top, None)?, frontier(&[("in-rec", "v1")]));
    let others = f.db.run_on_stack(
        "?[id, val] := *other{id, val @ 'NOW'}",
        Default::default(),
        &top,
        None,
        ScriptMutability::Immutable,
    )?;
    assert_eq!(others.rows.len(), 1);
    Ok(())
}

/// Bounded scans, the shape of upstream's stored-relation `prefix_join` regression
/// (commit `ff9a4fce`), behave across layers as they do on one.
#[test]
fn bounded_scans_compose_across_layers() -> Result<()> {
    let f = Fixture::new()?;
    let (base_stack, mid, top) = three_layers(&f)?;
    for id in ["a1", "a2", "b1", "b2", "c1"] {
        f.assert_rec(&base_stack, id, "base")?;
    }
    // Re-asserted higher up: the merge must still emit them once each.
    f.assert_rec(&mid, "b2", "base")?;
    f.assert_rec(&top, "c1", "base")?;

    let rows = f.db.run_on_stack(
        "?[id, val] := *rec{id, val @ 'NOW'}, id >= 'a2', id < 'c1' :order id",
        Default::default(),
        &top,
        None,
        ScriptMutability::Immutable,
    )?;
    let seen: Vec<(String, String)> = rows
        .rows
        .iter()
        .map(|r| (super::as_str(&r[0]), super::as_str(&r[1])))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("a2".to_string(), "base".to_string()),
            ("b1".to_string(), "base".to_string()),
            ("b2".to_string(), "base".to_string()),
        ]
    );
    Ok(())
}

/// Read results do not depend on the order in which layers were created.
#[test]
fn results_are_independent_of_layer_creation_order() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "shared", "base")?;
    let fork = f.seq();

    // Create the upper layer first, then the one that sits below it.
    f.db.create_layer("later")?;
    f.db.create_layer("earlier")?;
    let stack: Stack = vec![
        LayerRef::new("later"),
        LayerRef::new("earlier"),
        LayerRef::bounded("default", fork),
    ];
    f.assert_rec(
        &vec![LayerRef::new("earlier"), LayerRef::bounded("default", fork)],
        "from-earlier",
        "v1",
    )?;
    f.assert_rec(&stack, "from-later", "v2")?;

    assert_eq!(
        f.live(&stack, None)?,
        frontier(&[
            ("shared", "base"),
            ("from-earlier", "v1"),
            ("from-later", "v2")
        ])
    );
    Ok(())
}

/// A batched read agrees with the one-at-a-time read, for every position in the stack.
///
/// The batched path resolves a layer at a time rather than a key at a time, so it has its own
/// chance to get the shadowing order wrong, to mishandle a key no layer holds, or to ignore a
/// layer's window. It is checked against `get`, which is what the default implementation does.
#[test]
fn a_batched_read_agrees_with_reading_one_at_a_time() -> Result<()> {
    use crate::storage::{StoreTx, Storage};

    let f = Fixture::new()?;
    let (base_stack, mid, top) = three_layers(&f)?;
    f.assert_rec(&base_stack, "in-base", "vb")?;
    f.assert_rec(&mid, "in-mid", "vm")?;
    f.assert_rec(&top, "in-top", "vt")?;
    // Held by two layers at once, so the batched path has to prefer the topmost.
    f.assert_rec(&base_stack, "shared", "same")?;
    f.assert_rec(&top, "shared", "same")?;

    // Collect every stored key, plus one that does not exist.
    let spec = f.db.db.resolve(&top)?;
    let view = f.db.db.with_stack(spec);
    let tx = view.transact(false)?;
    let mut keys: Vec<Vec<u8>> = tx
        .range_scan(&[0u8; 8], &[0xffu8; 8])
        .map(|kv| kv.map(|(k, _)| k))
        .collect::<Result<Vec<_>>>()?;
    assert!(keys.len() >= 5, "expected the written rows, got {}", keys.len());
    let mut absent = keys[0].clone();
    *absent.last_mut().unwrap() = absent.last().unwrap().wrapping_add(1);
    keys.push(absent);

    let batched = tx.multi_get(&keys, false)?;
    let singly: Vec<Option<Vec<u8>>> = keys
        .iter()
        .map(|k| tx.get(k, false))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(batched, singly, "batched and single reads disagreed");
    Ok(())
}
