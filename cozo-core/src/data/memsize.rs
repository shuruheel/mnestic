/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared estimated-heap-size vocabulary (mnestic fork, query memory budget;
//! spec `docs/specs/memory-budget.md`). These estimators were born inside the
//! `graph-algo`-gated projection cache; the query memory budget needs them on
//! every build, so they live here un-gated and the projection cache imports
//! them. The figures are deliberately *estimates* of engine-held bytes — they
//! undercount allocator overhead by a roughly constant factor and are not
//! stable across releases; tests pin brackets, never exact bytes.

use crate::data::value::{DataValue, Vector};

/// Charged per `BTreeMap`/`BTreeSet` entry when estimating a store's size.
/// B-trees allocate in nodes, not per entry; this is the amortised share.
pub(crate) const BTREE_ENTRY_OVERHEAD: usize = 48;

/// Flat charge for the part of a value we cannot see into: a compiled
/// regex's program. Its pattern text is counted exactly; the compiled
/// automaton is opaque, and the estimate only needs the right order of
/// magnitude for a value type this pathological.
pub(crate) const OPAQUE_KEY_ESTIMATE: usize = 64;

/// Heap bytes owned by a value, beyond its inline `DataValue`.
pub(crate) fn value_heap_bytes(v: &DataValue) -> usize {
    match v {
        DataValue::Str(s) => {
            if s.is_inline() {
                0
            } else {
                s.len()
            }
        }
        DataValue::Bytes(b) => b.len(),
        DataValue::List(l) => {
            l.len() * std::mem::size_of::<DataValue>()
                + l.iter().map(value_heap_bytes).sum::<usize>()
        }
        DataValue::Set(s) => s
            .iter()
            .map(|v| std::mem::size_of::<DataValue>() + BTREE_ENTRY_OVERHEAD + value_heap_bytes(v))
            .sum(),
        DataValue::Vec(Vector::F32(a)) => a.len() * 4,
        DataValue::Vec(Vector::F64(a)) => a.len() * 8,
        // Json must be walked, not flat-charged: a single blob can be
        // megabytes (projection-cache Phase 3/4 review, 2026-07-10).
        DataValue::Json(j) => json_heap_bytes(&j.0),
        DataValue::Regex(r) => r.0.as_str().len() + OPAQUE_KEY_ESTIMATE,
        _ => 0,
    }
}

/// Heap bytes owned by a JSON value, beyond one inline `serde_json::Value`.
pub(crate) fn json_heap_bytes(v: &serde_json::Value) -> usize {
    let node = std::mem::size_of::<serde_json::Value>();
    match v {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(a) => {
            a.len() * node + a.iter().map(json_heap_bytes).sum::<usize>()
        }
        serde_json::Value::Object(m) => m
            .iter()
            .map(|(k, x)| k.len() + node + BTREE_ENTRY_OVERHEAD + json_heap_bytes(x))
            .sum(),
        _ => 0,
    }
}

/// Estimated owned bytes of one tuple: the `Vec` header, one inline
/// `DataValue` per column, and each value's heap. Does NOT include the
/// per-entry `BTREE_ENTRY_OVERHEAD` — the charge site adds that, because a
/// tuple used as a map *value* inside an existing entry carries no own entry.
pub(crate) fn est_tuple_bytes(t: &[DataValue]) -> usize {
    std::mem::size_of::<Vec<DataValue>>()
        + std::mem::size_of_val(t)
        + t.iter().map(value_heap_bytes).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_tuple_lands_in_the_measured_bracket() {
        // The audit measured ~168.4 B/row for the narrowest 2-col tuple
        // including B-tree overhead; the estimate must land in that
        // neighborhood (bracket, not exact bytes — the estimate is
        // documented as unstable across releases).
        let t = vec![DataValue::from(1i64), DataValue::from(2i64)];
        let est = est_tuple_bytes(&t) + BTREE_ENTRY_OVERHEAD;
        assert!(
            (120..=240).contains(&est),
            "2-col int tuple estimated at {est} B; expected the ~168 B bracket"
        );
    }

    #[test]
    fn vector_tuple_charges_its_heap() {
        let v = DataValue::Vec(Vector::F32(ndarray::Array1::zeros(1536)));
        let t = vec![DataValue::from(1i64), v];
        let est = est_tuple_bytes(&t);
        assert!(
            est >= 1536 * 4,
            "a 1536-dim f32 vector tuple must charge at least its heap ({est} B)"
        );
    }

    #[test]
    fn string_heap_only_when_not_inline() {
        let long = DataValue::Str("a-string-well-past-inline-capacity-for-sure".into());
        let t = vec![long];
        assert!(est_tuple_bytes(&t) > est_tuple_bytes(&[DataValue::from(1i64)]));
    }
}
