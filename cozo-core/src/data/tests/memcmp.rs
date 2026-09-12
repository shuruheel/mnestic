/*
 *  Copyright 2022, The Cozo Project Authors.
 *
 *  This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 *  If a copy of the MPL was not distributed with this file,
 *  You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 */

use uuid::Uuid;

use crate::data::memcmp::{decode_bytes, MemCmpEncoder};
use crate::data::value::{DataValue, Num, UuidWrapper};

#[test]
fn encode_decode_num() {
    use rand::prelude::*;

    let n = i64::MAX;
    let mut collected = vec![];

    let mut test_num = |n: Num| {
        let mut encoder = vec![];
        encoder.encode_num(n);
        let (decoded, rest) = Num::decode_from_key(&encoder);
        assert_eq!(decoded, n);
        assert!(rest.is_empty());
        collected.push(encoder);
    };
    for i in 0..54 {
        for j in 0..1000 {
            let vb = (n >> i) - j;
            for v in [vb, -vb - 1] {
                test_num(Num::Int(v));
            }
        }
    }
    test_num(Num::Float(f64::INFINITY));
    test_num(Num::Float(f64::NEG_INFINITY));
    test_num(Num::Float(f64::NAN));
    for _ in 0..100000 {
        let f = (thread_rng().gen::<f64>() - 0.5) * 2.0;
        test_num(Num::Float(f));
        test_num(Num::Float(1. / f));
    }
    let mut collected_copy = collected.clone();
    collected.sort();
    collected_copy.sort_by_key(|c| Num::decode_from_key(c).0);
    assert_eq!(collected, collected_copy);
}

#[test]
fn test_encode_decode_uuid() {
    let uuid = DataValue::Uuid(UuidWrapper(
        Uuid::parse_str("dd85b19a-5fde-11ed-a88e-1774a7698039").unwrap(),
    ));
    let mut encoder = vec![];
    encoder.encode_datavalue(&uuid);
    let (decoded, remaining) = DataValue::decode_from_key(&encoder);
    assert_eq!(decoded, uuid);
    assert!(remaining.is_empty());
}

#[test]
fn encode_decode_bytes() {
    let target = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit...";
    for i in 0..target.len() {
        let bs = &target[i..];
        let mut encoder: Vec<u8> = vec![];
        encoder.encode_bytes(bs);
        let (decoded, remaining) = decode_bytes(&encoder);
        assert!(remaining.is_empty());
        assert_eq!(bs, decoded);

        let mut encoder: Vec<u8> = vec![];
        encoder.encode_bytes(target);
        encoder.encode_bytes(bs);
        encoder.encode_bytes(bs);
        encoder.encode_bytes(target);

        let (decoded, remaining) = decode_bytes(&encoder);
        assert_eq!(&target[..], decoded);

        let (decoded, remaining) = decode_bytes(remaining);
        assert_eq!(bs, decoded);

        let (decoded, remaining) = decode_bytes(remaining);
        assert_eq!(bs, decoded);

        let (decoded, remaining) = decode_bytes(remaining);
        assert_eq!(&target[..], decoded);
        assert!(remaining.is_empty());
    }
}

#[test]
fn specific_encode() {
    let mut encoder = vec![];
    encoder.encode_datavalue(&DataValue::from(2095));
    // println!("e1 {:?}", encoder);
    encoder.encode_datavalue(&DataValue::from("MSS"));
    // println!("e2 {:?}", encoder);
    let (a, remaining) = DataValue::decode_from_key(&encoder);
    // println!("r  {:?}", remaining);
    let (b, remaining) = DataValue::decode_from_key(remaining);
    assert!(remaining.is_empty());
    assert_eq!(a, DataValue::from(2095));
    assert_eq!(b, DataValue::from("MSS"));
}

#[test]
fn encode_decode_datavalues() {
    let mut dv = vec![
        DataValue::Null,
        DataValue::from(false),
        DataValue::from(true),
        DataValue::from(1),
        DataValue::from(1.0),
        DataValue::from(i64::MAX),
        DataValue::from(i64::MAX - 1),
        DataValue::from(i64::MAX - 2),
        DataValue::from(i64::MIN),
        DataValue::from(i64::MIN + 1),
        DataValue::from(i64::MIN + 2),
        DataValue::from(f64::INFINITY),
        DataValue::from(f64::NEG_INFINITY),
        DataValue::List(vec![]),
    ];
    dv.push(DataValue::List(dv.clone()));
    dv.push(DataValue::List(dv.clone()));
    let mut encoded = vec![];
    let v = DataValue::List(dv);
    encoded.encode_datavalue(&v);
    let (decoded, remaining) = DataValue::decode_from_key(&encoded);
    assert!(remaining.is_empty());
    assert_eq!(decoded, v);
}

/// An encoded validity, ten bytes: tag, order-encoded timestamp, assertion byte.
fn encoded_validity(ts: i64) -> Vec<u8> {
    use crate::data::value::{Validity, ValidityTs};
    use std::cmp::Reverse;
    let mut out = vec![];
    out.encode_datavalue(&DataValue::Validity(Validity {
        timestamp: ValidityTs(Reverse(ts)),
        is_assert: Reverse(true),
    }));
    out
}

#[test]
fn validity_markers_are_found_wherever_they_sit() {
    use crate::data::memcmp::contains_validity_ts;

    let vld = encoded_validity(7);
    assert!(contains_validity_ts(&vld, 7));
    assert!(!contains_validity_ts(&vld, 8), "a different stamp matched");

    // At the very start, at the very end, and surrounded.
    let mut at_end = vec![0xAA; 5];
    at_end.extend_from_slice(&vld);
    assert!(contains_validity_ts(&at_end, 7));

    let mut surrounded = vec![0xAA; 3];
    surrounded.extend_from_slice(&vld);
    surrounded.extend_from_slice(&[0xBB; 4]);
    assert!(contains_validity_ts(&surrounded, 7));

    // Shorter than a marker, and empty.
    assert!(!contains_validity_ts(&vld[..4], 7));
    assert!(!contains_validity_ts(&[], 7));
}

#[test]
fn a_near_miss_does_not_match() {
    use crate::data::memcmp::contains_validity_ts;

    // The tag byte is present but the timestamp that follows is a different one, so the
    // candidate must be rejected and the search must continue past it.
    let mut buf = encoded_validity(11);
    buf.extend_from_slice(&encoded_validity(7));
    assert!(contains_validity_ts(&buf, 7), "the second marker was missed");
    assert!(contains_validity_ts(&buf, 11));
    assert!(!contains_validity_ts(&buf, 9));
}

#[test]
fn restamping_rewrites_every_occurrence() {
    use crate::data::memcmp::{contains_validity_ts, restamp_all_validity};

    // Three markers: at the start, adjacent to the second, and after a gap.
    let mut buf = encoded_validity(7);
    buf.extend_from_slice(&encoded_validity(7));
    buf.extend_from_slice(&[0xAA; 6]);
    buf.extend_from_slice(&encoded_validity(7));
    let before = buf.clone();

    assert_eq!(restamp_all_validity(&mut buf, 7, 42), 3);
    assert!(!contains_validity_ts(&buf, 7), "an old stamp survived");
    assert!(contains_validity_ts(&buf, 42));
    assert_eq!(buf.len(), before.len(), "restamping changed the length");

    // Restamping a stamp that is not there rewrites nothing and changes nothing.
    let untouched = buf.clone();
    assert_eq!(restamp_all_validity(&mut buf, 7, 99), 0);
    assert_eq!(buf, untouched);
}
