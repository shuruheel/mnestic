/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for the mnestic fork's ULID functions (`rand_ulid`, `ulid_timestamp`),
//! upstream cozo #296: lexicographically-sortable identifiers.

use cozo::{DataValue, DbInstance, ScriptMutability};
use std::collections::BTreeMap;

fn scalar(db: &DbInstance, script: &str) -> DataValue {
    let res = db
        .run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n{script}"));
    res.rows[0][0].clone()
}

const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[test]
fn rand_ulid_has_canonical_format() {
    let db = DbInstance::default();
    let v = scalar(&db, "?[u] := u = rand_ulid()");
    let s = v.get_str().expect("rand_ulid returns a string");
    assert_eq!(s.len(), 26, "ULID is 26 chars, got {s:?}");
    assert!(
        s.chars().all(|c| CROCKFORD.contains(c)),
        "ULID must use the Crockford alphabet: {s:?}"
    );
}

#[test]
fn ulid_timestamp_decodes_known_vectors() {
    let db = DbInstance::default();
    // A '1' in the 32^16 place encodes the value 2^80, whose high-48-bit
    // timestamp is exactly 1. (10-char timestamp section + 16-char randomness.)
    let ts1 = scalar(&db, "?[t] := t = ulid_timestamp('00000000010000000000000000')");
    assert_eq!(ts1, DataValue::from(1i64));

    // A real ULID: its first 10 chars '01ARZ3NDEK' decode (Crockford base32) to
    // 1469922850259 — verifiable by hand: ((((((((1*32+10)*32+24)*32+31)*32+3)
    // *32+21)*32+13)*32+14)*32+19) over digits [0,1,10,24,31,3,21,13,14,19].
    let ts2 = scalar(&db, "?[t] := t = ulid_timestamp('01ARZ3NDEKTSV4RRFFQ69G5FAV')");
    assert_eq!(ts2, DataValue::from(1469922850259i64));
}

#[test]
fn ulid_timestamp_zero() {
    let db = DbInstance::default();
    let ts = scalar(&db, "?[t] := t = ulid_timestamp('00000000000000000000000000')");
    assert_eq!(ts, DataValue::from(0i64));
}

#[test]
fn rand_ulid_timestamp_is_recent() {
    let db = DbInstance::default();
    let ts = scalar(&db, "?[t] := u = rand_ulid(), t = ulid_timestamp(u)")
        .get_int()
        .expect("ulid_timestamp returns an int");
    // Between 2020-01-01 and 2100-01-01 (ms). Proves the timestamp half encodes
    // wall-clock time correctly.
    assert!(
        (1_577_836_800_000..4_102_444_800_000).contains(&ts),
        "ULID timestamp {ts} ms is not a plausible current time"
    );
}

#[test]
fn rand_ulid_is_sortable_and_distinct() {
    let db = DbInstance::default();
    // A fresh ULID sorts after the all-zero (epoch) ULID lexicographically.
    let cmp = scalar(
        &db,
        "?[b] := u = rand_ulid(), b = ('00000000000000000000000000' < u)",
    );
    assert_eq!(cmp, DataValue::Bool(true), "now-ULID must sort after epoch-ULID");

    // Two ULIDs are distinct (randomness half differs even within a ms).
    let distinct = scalar(
        &db,
        "?[b] := a = rand_ulid(), c = rand_ulid(), b = (a != c)",
    );
    assert_eq!(distinct, DataValue::Bool(true));
}
