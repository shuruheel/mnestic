/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use approx::AbsDiffEq;
use num_traits::FloatConst;
use regex::Regex;
use serde_json::json;

use crate::data::functions::*;
use crate::data::value::{DataValue, RegexWrapper};
use crate::DbInstance;

#[test]
fn test_add() {
    assert_eq!(op_add(&[]).unwrap(), DataValue::from(0));
    assert_eq!(op_add(&[DataValue::from(1)]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_add(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(3)
    );
    assert_eq!(
        op_add(&[DataValue::from(1), DataValue::from(2.5)]).unwrap(),
        DataValue::from(3.5)
    );
    assert_eq!(
        op_add(&[DataValue::from(1.5), DataValue::from(2.5)]).unwrap(),
        DataValue::from(4.0)
    );
}

#[test]
fn test_sub() {
    assert_eq!(
        op_sub(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_sub(&[DataValue::from(1), DataValue::from(2.5)]).unwrap(),
        DataValue::from(-1.5)
    );
    assert_eq!(
        op_sub(&[DataValue::from(1.5), DataValue::from(2.5)]).unwrap(),
        DataValue::from(-1.0)
    );
}

#[test]
fn test_mul() {
    assert_eq!(op_mul(&[]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_mul(&[DataValue::from(2), DataValue::from(3)]).unwrap(),
        DataValue::from(6)
    );
    assert_eq!(
        op_mul(&[DataValue::from(0.5), DataValue::from(0.25)]).unwrap(),
        DataValue::from(0.125)
    );
    assert_eq!(
        op_mul(&[DataValue::from(0.5), DataValue::from(3)]).unwrap(),
        DataValue::from(1.5)
    );
}

#[test]
fn test_div() {
    assert_eq!(
        op_div(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_div(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(0.5)
    );
    assert_eq!(
        op_div(&[DataValue::from(7.0), DataValue::from(0.5)]).unwrap(),
        DataValue::from(14.0)
    );
    assert!(op_div(&[DataValue::from(1), DataValue::from(0)]).is_ok());
}

#[test]
fn test_eq_neq() {
    assert_eq!(
        op_eq(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_eq(&[DataValue::from(123), DataValue::from(123)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_neq(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_neq(&[DataValue::from(123), DataValue::from(123.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_eq(&[DataValue::from(123), DataValue::from(123.1)]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_list() {
    assert_eq!(op_list(&[]).unwrap(), DataValue::List(vec![]));
    assert_eq!(
        op_list(&[DataValue::from(1)]).unwrap(),
        DataValue::List(vec![DataValue::from(1)])
    );
    assert_eq!(
        op_list(&[DataValue::from(1), DataValue::List(vec![])]).unwrap(),
        DataValue::List(vec![DataValue::from(1), DataValue::List(vec![])])
    );
}

#[test]
fn test_is_in() {
    assert_eq!(
        op_is_in(&[
            DataValue::from(1),
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)])
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_in(&[
            DataValue::from(3),
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)])
        ])
        .unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_in(&[DataValue::from(3), DataValue::List(vec![])]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_comparators() {
    assert_eq!(
        op_ge(&[DataValue::from(2), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(2.), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(2), DataValue::from(1.)]).unwrap(),
        DataValue::from(true)
    );

    assert_eq!(
        op_ge(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(false)
    );
    assert!(op_ge(&[DataValue::Null, DataValue::from(true)]).is_err());
    assert_eq!(
        op_gt(&[DataValue::from(2), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_gt(&[DataValue::from(2.), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_gt(&[DataValue::from(2), DataValue::from(1.)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_gt(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_gt(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_gt(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(false)
    );
    assert!(op_gt(&[DataValue::Null, DataValue::from(true)]).is_err());
    assert_eq!(
        op_le(&[DataValue::from(2), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_le(&[DataValue::from(2.), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_le(&[DataValue::from(2), DataValue::from(1.)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_le(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_le(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_le(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(true)
    );
    assert!(op_le(&[DataValue::Null, DataValue::from(true)]).is_err());
    assert_eq!(
        op_lt(&[DataValue::from(2), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(2.), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(2), DataValue::from(1.)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(true)
    );
    assert!(op_lt(&[DataValue::Null, DataValue::from(true)]).is_err());
}

#[test]
fn test_max_min() {
    assert_eq!(op_max(&[DataValue::from(1),]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_max(&[
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4)
        ])
        .unwrap(),
        DataValue::from(4)
    );
    assert_eq!(
        op_max(&[
            DataValue::from(1.0),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4)
        ])
        .unwrap(),
        DataValue::from(4)
    );
    assert_eq!(
        op_max(&[
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4.0)
        ])
        .unwrap(),
        DataValue::from(4.0)
    );
    assert!(op_max(&[DataValue::from(true)]).is_err());

    assert_eq!(op_min(&[DataValue::from(1),]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_min(&[
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4)
        ])
        .unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_min(&[
            DataValue::from(1.0),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4)
        ])
        .unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_min(&[
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4.0)
        ])
        .unwrap(),
        DataValue::from(1)
    );
    assert!(op_max(&[DataValue::from(true)]).is_err());
}

#[test]
fn test_minus() {
    assert_eq!(
        op_minus(&[DataValue::from(-1)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_minus(&[DataValue::from(1)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_minus(&[DataValue::from(f64::INFINITY)]).unwrap(),
        DataValue::from(f64::NEG_INFINITY)
    );
    assert_eq!(
        op_minus(&[DataValue::from(f64::NEG_INFINITY)]).unwrap(),
        DataValue::from(f64::INFINITY)
    );
}

#[test]
fn test_abs() {
    assert_eq!(op_abs(&[DataValue::from(-1)]).unwrap(), DataValue::from(1));
    assert_eq!(op_abs(&[DataValue::from(1)]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_abs(&[DataValue::from(-1.5)]).unwrap(),
        DataValue::from(1.5)
    );
}

#[test]
fn test_signum() {
    assert_eq!(
        op_signum(&[DataValue::from(0.1)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_signum(&[DataValue::from(-0.1)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_signum(&[DataValue::from(0.0)]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_signum(&[DataValue::from(-0.0)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_signum(&[DataValue::from(-3)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_signum(&[DataValue::from(f64::NEG_INFINITY)]).unwrap(),
        DataValue::from(-1)
    );
    assert!(op_signum(&[DataValue::from(f64::NAN)])
        .unwrap()
        .get_float()
        .unwrap()
        .is_nan());
}

#[test]
fn test_floor_ceil() {
    assert_eq!(
        op_floor(&[DataValue::from(-1)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_floor(&[DataValue::from(-1.5)]).unwrap(),
        DataValue::from(-2.0)
    );
    assert_eq!(
        op_floor(&[DataValue::from(1.5)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_ceil(&[DataValue::from(-1)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_ceil(&[DataValue::from(-1.5)]).unwrap(),
        DataValue::from(-1.0)
    );
    assert_eq!(
        op_ceil(&[DataValue::from(1.5)]).unwrap(),
        DataValue::from(2.0)
    );
}

#[test]
fn test_round() {
    assert_eq!(
        op_round(&[DataValue::from(0.6)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(0.5)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(1.5)]).unwrap(),
        DataValue::from(2.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(-0.6)]).unwrap(),
        DataValue::from(-1.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(-0.5)]).unwrap(),
        DataValue::from(-1.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(-1.5)]).unwrap(),
        DataValue::from(-2.0)
    );
}

#[test]
fn test_exp() {
    let n = op_exp(&[DataValue::from(1)]).unwrap().get_float().unwrap();
    assert!(n.abs_diff_eq(&f64::E(), 1E-5));

    let n = op_exp(&[DataValue::from(50.1)])
        .unwrap()
        .get_float()
        .unwrap();
    assert!(n.abs_diff_eq(&(50.1_f64.exp()), 1E-5));
}

#[test]
fn test_exp2() {
    let n = op_exp2(&[DataValue::from(10.)])
        .unwrap()
        .get_float()
        .unwrap();
    assert_eq!(n, 1024.);
}

#[test]
fn test_ln() {
    assert_eq!(
        op_ln(&[DataValue::from(f64::E())]).unwrap(),
        DataValue::from(1.0)
    );
}

#[test]
fn test_log2() {
    assert_eq!(
        op_log2(&[DataValue::from(1024)]).unwrap(),
        DataValue::from(10.)
    );
}

#[test]
fn test_log10() {
    assert_eq!(
        op_log10(&[DataValue::from(1000)]).unwrap(),
        DataValue::from(3.0)
    );
}

#[test]
fn test_trig() {
    assert!(op_sin(&[DataValue::from(f64::PI() / 2.)])
        .unwrap()
        .get_float()
        .unwrap()
        .abs_diff_eq(&1.0, 1e-5));
    assert!(op_cos(&[DataValue::from(f64::PI() / 2.)])
        .unwrap()
        .get_float()
        .unwrap()
        .abs_diff_eq(&0.0, 1e-5));
    assert!(op_tan(&[DataValue::from(f64::PI() / 4.)])
        .unwrap()
        .get_float()
        .unwrap()
        .abs_diff_eq(&1.0, 1e-5));
}

#[test]
fn test_inv_trig() {
    assert!(op_asin(&[DataValue::from(1.0)])
        .unwrap()
        .get_float()
        .unwrap()
        .abs_diff_eq(&(f64::PI() / 2.), 1e-5));
    assert!(op_acos(&[DataValue::from(0)])
        .unwrap()
        .get_float()
        .unwrap()
        .abs_diff_eq(&(f64::PI() / 2.), 1e-5));
    assert!(op_atan(&[DataValue::from(1)])
        .unwrap()
        .get_float()
        .unwrap()
        .abs_diff_eq(&(f64::PI() / 4.), 1e-5));
    assert!(op_atan2(&[DataValue::from(-1), DataValue::from(-1)])
        .unwrap()
        .get_float()
        .unwrap()
        .abs_diff_eq(&(-3. * f64::PI() / 4.), 1e-5));
}

#[test]
fn test_pow() {
    assert_eq!(
        op_pow(&[DataValue::from(2), DataValue::from(10)]).unwrap(),
        DataValue::from(1024.0)
    );
}

#[test]
fn test_mod() {
    assert_eq!(
        op_mod(&[DataValue::from(-10), DataValue::from(7)]).unwrap(),
        DataValue::from(-3)
    );
    assert!(op_mod(&[DataValue::from(5), DataValue::from(0.)]).is_ok());
    assert!(op_mod(&[DataValue::from(5.), DataValue::from(0.)]).is_ok());
    assert!(op_mod(&[DataValue::from(5.), DataValue::from(0)]).is_ok());
    assert!(op_mod(&[DataValue::from(5), DataValue::from(0)]).is_err());
}

#[test]
fn test_boolean() {
    assert_eq!(op_and(&[]).unwrap(), DataValue::from(true));
    assert_eq!(
        op_and(&[DataValue::from(true), DataValue::from(false)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(op_or(&[]).unwrap(), DataValue::from(false));
    assert_eq!(
        op_or(&[DataValue::from(true), DataValue::from(false)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_negate(&[DataValue::from(false)]).unwrap(),
        DataValue::from(true)
    );
}

#[test]
fn test_bits() {
    assert_eq!(
        op_bit_and(&[
            DataValue::Bytes([0b111000].into()),
            DataValue::Bytes([0b010101].into())
        ])
        .unwrap(),
        DataValue::Bytes([0b010000].into())
    );
    assert_eq!(
        op_bit_or(&[
            DataValue::Bytes([0b111000].into()),
            DataValue::Bytes([0b010101].into())
        ])
        .unwrap(),
        DataValue::Bytes([0b111101].into())
    );
    assert_eq!(
        op_bit_not(&[DataValue::Bytes([0b00111000].into())]).unwrap(),
        DataValue::Bytes([0b11000111].into())
    );
    assert_eq!(
        op_bit_xor(&[
            DataValue::Bytes([0b111000].into()),
            DataValue::Bytes([0b010101].into())
        ])
        .unwrap(),
        DataValue::Bytes([0b101101].into())
    );
}

#[test]
fn test_pack_bits() {
    assert_eq!(
        op_pack_bits(&[DataValue::List(vec![DataValue::from(true)])]).unwrap(),
        DataValue::Bytes([0b10000000].into())
    )
}

#[test]
fn test_unpack_bits() {
    assert_eq!(
        op_unpack_bits(&[DataValue::Bytes([0b10101010].into())]).unwrap(),
        DataValue::List(
            [true, false, true, false, true, false, true, false]
                .into_iter()
                .map(DataValue::Bool)
                .collect()
        )
    )
}

#[test]
fn test_concat() {
    assert_eq!(
        op_concat(&[DataValue::Str("abc".into()), DataValue::Str("def".into())]).unwrap(),
        DataValue::Str("abcdef".into())
    );

    assert_eq!(
        op_concat(&[
            DataValue::List(vec![DataValue::from(true), DataValue::from(false)]),
            DataValue::List(vec![DataValue::from(true)])
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::from(true),
            DataValue::from(false),
            DataValue::from(true),
        ])
    );
}

#[test]
fn test_str_includes() {
    assert_eq!(
        op_str_includes(&[
            DataValue::Str("abcdef".into()),
            DataValue::Str("bcd".into())
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_str_includes(&[DataValue::Str("abcdef".into()), DataValue::Str("bd".into())]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_casings() {
    assert_eq!(
        op_lowercase(&[DataValue::Str("NAÏVE".into())]).unwrap(),
        DataValue::Str("naïve".into())
    );
    assert_eq!(
        op_uppercase(&[DataValue::Str("naïve".into())]).unwrap(),
        DataValue::Str("NAÏVE".into())
    );
}

#[test]
fn test_trim() {
    assert_eq!(
        op_trim(&[DataValue::Str(" a ".into())]).unwrap(),
        DataValue::Str("a".into())
    );
    assert_eq!(
        op_trim_start(&[DataValue::Str(" a ".into())]).unwrap(),
        DataValue::Str("a ".into())
    );
    assert_eq!(
        op_trim_end(&[DataValue::Str(" a ".into())]).unwrap(),
        DataValue::Str(" a".into())
    );
}

#[test]
fn test_starts_ends_with() {
    assert_eq!(
        op_starts_with(&[
            DataValue::Str("abcdef".into()),
            DataValue::Str("abc".into())
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_starts_with(&[DataValue::Str("abcdef".into()), DataValue::Str("bc".into())]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_ends_with(&[
            DataValue::Str("abcdef".into()),
            DataValue::Str("def".into())
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ends_with(&[DataValue::Str("abcdef".into()), DataValue::Str("bc".into())]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_regex() {
    assert_eq!(
        op_regex_matches(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("c.e").unwrap()))
        ])
        .unwrap(),
        DataValue::from(true)
    );

    assert_eq!(
        op_regex_matches(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("c.ef$").unwrap()))
        ])
        .unwrap(),
        DataValue::from(true)
    );

    assert_eq!(
        op_regex_matches(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("c.e$").unwrap()))
        ])
        .unwrap(),
        DataValue::from(false)
    );

    assert_eq!(
        op_regex_replace(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("[be]").unwrap())),
            DataValue::Str("x".into())
        ])
        .unwrap(),
        DataValue::Str("axcdef".into())
    );

    assert_eq!(
        op_regex_replace_all(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("[be]").unwrap())),
            DataValue::Str("x".into())
        ])
        .unwrap(),
        DataValue::Str("axcdxf".into())
    );
    assert_eq!(
        op_regex_extract(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(RegexWrapper(Regex::new("[xayef]|(GH)").unwrap()))
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Str("a".into()),
            DataValue::Str("e".into()),
            DataValue::Str("f".into()),
            DataValue::Str("GH".into()),
        ])
    );
    assert_eq!(
        op_regex_extract_first(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(RegexWrapper(Regex::new("[xayef]|(GH)").unwrap()))
        ])
        .unwrap(),
        DataValue::Str("a".into()),
    );
    assert_eq!(
        op_regex_extract(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(RegexWrapper(Regex::new("xyz").unwrap()))
        ])
        .unwrap(),
        DataValue::List(vec![])
    );

    assert_eq!(
        op_regex_extract_first(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(RegexWrapper(Regex::new("xyz").unwrap()))
        ])
        .unwrap(),
        DataValue::Null
    );
}

#[test]
fn test_predicates() {
    assert_eq!(
        op_is_null(&[DataValue::Null]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_null(&[DataValue::Bot]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_int(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_int(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_float(&[DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_float(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_num(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_num(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_num(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_bytes(&[DataValue::Bytes([0b1].into())]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_bytes(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_list(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_list(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_string(&[DataValue::Str("".into())]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_string(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_finite(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_finite(&[DataValue::from(f64::INFINITY)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_finite(&[DataValue::from(f64::NAN)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(f64::INFINITY)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(f64::NEG_INFINITY)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(f64::NAN)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(f64::INFINITY)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(f64::NEG_INFINITY)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(f64::NAN)]).unwrap(),
        DataValue::from(true)
    );
}

#[test]
fn test_prepend_append() {
    assert_eq!(
        op_prepend(&[
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::Null,
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Null,
            DataValue::from(1),
            DataValue::from(2),
        ]),
    );
    assert_eq!(
        op_append(&[
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::Null,
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
            DataValue::Null,
        ]),
    );
}

#[test]
fn test_length() {
    assert_eq!(
        op_length(&[DataValue::Str("abc".into())]).unwrap(),
        DataValue::from(3)
    );
    assert_eq!(
        op_length(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_length(&[DataValue::Bytes([].into())]).unwrap(),
        DataValue::from(0)
    );
}

#[test]
fn test_unicode_normalize() {
    assert_eq!(
        op_unicode_normalize(&[DataValue::Str("abc".into()), DataValue::Str("nfc".into())])
            .unwrap(),
        DataValue::Str("abc".into())
    )
}

#[test]
fn test_sort_reverse() {
    assert_eq!(
        op_sorted(&[DataValue::List(vec![
            DataValue::from(2.0),
            DataValue::from(1),
            DataValue::from(2),
            DataValue::Null,
        ])])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Null,
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(2.0),
        ])
    );
    assert_eq!(
        op_reverse(&[DataValue::List(vec![
            DataValue::from(2.0),
            DataValue::from(1),
            DataValue::from(2),
            DataValue::Null,
        ])])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Null,
            DataValue::from(2),
            DataValue::from(1),
            DataValue::from(2.0),
        ])
    )
}

#[test]
fn test_haversine() {
    let d = op_haversine_deg_input(&[
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(180),
    ])
    .unwrap()
    .get_float()
    .unwrap();
    assert!(d.abs_diff_eq(&f64::PI(), 1e-5));

    let d = op_haversine_deg_input(&[
        DataValue::from(90),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(123),
    ])
    .unwrap()
    .get_float()
    .unwrap();
    assert!(d.abs_diff_eq(&(f64::PI() / 2.), 1e-5));

    let d = op_haversine(&[
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(f64::PI()),
    ])
    .unwrap()
    .get_float()
    .unwrap();
    assert!(d.abs_diff_eq(&f64::PI(), 1e-5));
}

#[test]
fn test_deg_rad() {
    assert_eq!(
        op_deg_to_rad(&[DataValue::from(180)]).unwrap(),
        DataValue::from(f64::PI())
    );
    assert_eq!(
        op_rad_to_deg(&[DataValue::from(f64::PI())]).unwrap(),
        DataValue::from(180.0)
    );
}

#[test]
fn test_first_last() {
    assert_eq!(
        op_first(&[DataValue::List(vec![])]).unwrap(),
        DataValue::Null,
    );
    assert_eq!(
        op_last(&[DataValue::List(vec![])]).unwrap(),
        DataValue::Null,
    );
    assert_eq!(
        op_first(&[DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
        ])])
        .unwrap(),
        DataValue::from(1),
    );
    assert_eq!(
        op_last(&[DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
        ])])
        .unwrap(),
        DataValue::from(2),
    );
}

#[test]
fn test_chunks() {
    assert_eq!(
        op_chunks(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
            DataValue::from(2),
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::List(vec![DataValue::from(3), DataValue::from(4)]),
            DataValue::List(vec![DataValue::from(5)]),
        ])
    );
    assert_eq!(
        op_chunks_exact(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
            DataValue::from(2),
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::List(vec![DataValue::from(3), DataValue::from(4)]),
        ])
    );
    assert_eq!(
        op_windows(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
            DataValue::from(3),
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::List(vec![
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
            ]),
            DataValue::List(vec![
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
        ])
    )
}

#[test]
fn test_get() {
    assert!(op_get(&[DataValue::List(vec![]), DataValue::from(0)]).is_err());
    assert_eq!(
        op_get(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1)
        ])
        .unwrap(),
        DataValue::from(2)
    );
    assert_eq!(
        op_maybe_get(&[DataValue::List(vec![]), DataValue::from(0)]).unwrap(),
        DataValue::Null
    );
    assert_eq!(
        op_maybe_get(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1)
        ])
        .unwrap(),
        DataValue::from(2)
    );
}

#[test]
fn test_slice() {
    assert!(op_slice(&[
        DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
        ]),
        DataValue::from(1),
        DataValue::from(4)
    ])
    .is_err());

    assert!(op_slice(&[
        DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
        ]),
        DataValue::from(1),
        DataValue::from(3)
    ])
    .is_ok());

    assert_eq!(
        op_slice(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1),
            DataValue::from(-1)
        ])
        .unwrap(),
        DataValue::List(vec![DataValue::from(2)])
    );
}

#[test]
fn test_chars() {
    assert_eq!(
        op_from_substrings(&[op_chars(&[DataValue::Str("abc".into())]).unwrap()]).unwrap(),
        DataValue::Str("abc".into())
    )
}

#[test]
fn test_encode_decode() {
    assert_eq!(
        op_decode_base64(&[op_encode_base64(&[DataValue::Bytes([1, 2, 3].into())]).unwrap()])
            .unwrap(),
        DataValue::Bytes([1, 2, 3].into())
    )
}

#[test]
fn test_to_string() {
    assert_eq!(
        op_to_string(&[DataValue::from(false)]).unwrap(),
        DataValue::Str("false".into())
    );
}

#[test]
fn test_to_unity() {
    assert_eq!(op_to_unity(&[DataValue::Null]).unwrap(), DataValue::from(0));
    assert_eq!(
        op_to_unity(&[DataValue::from(false)]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(true)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(10)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(f64::NAN)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::Str("0".into())]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::Str("".into())]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::List(vec![DataValue::Null])]).unwrap(),
        DataValue::from(1)
    );
}

#[test]
fn test_to_float() {
    assert_eq!(
        op_to_float(&[DataValue::Null]).unwrap(),
        DataValue::from(0.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(false)]).unwrap(),
        DataValue::from(0.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(true)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(1)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(1.0)
    );
    assert!(op_to_float(&[DataValue::Str("NAN".into())])
        .unwrap()
        .get_float()
        .unwrap()
        .is_nan());
    assert!(op_to_float(&[DataValue::Str("INF".into())])
        .unwrap()
        .get_float()
        .unwrap()
        .is_infinite());
    assert!(op_to_float(&[DataValue::Str("NEG_INF".into())])
        .unwrap()
        .get_float()
        .unwrap()
        .is_infinite());
    assert_eq!(
        op_to_float(&[DataValue::Str("3".into())])
            .unwrap()
            .get_float()
            .unwrap(),
        3.
    );
}

#[test]
fn test_rand() {
    let n = op_rand_float(&[]).unwrap().get_float().unwrap();
    assert!(n >= 0.);
    assert!(n <= 1.);
    assert_eq!(
        op_rand_bernoulli(&[DataValue::from(0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_rand_bernoulli(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert!(op_rand_bernoulli(&[DataValue::from(2)]).is_err());
    let n = op_rand_int(&[DataValue::from(100), DataValue::from(200)])
        .unwrap()
        .get_int()
        .unwrap();
    assert!(n >= 100);
    assert!(n <= 200);
    assert_eq!(
        op_rand_choose(&[DataValue::List(vec![])]).unwrap(),
        DataValue::Null
    );
    assert_eq!(
        op_rand_choose(&[DataValue::List(vec![DataValue::from(123)])]).unwrap(),
        DataValue::from(123)
    );
}

#[test]
fn test_set_ops() {
    assert_eq!(
        op_union(&[
            DataValue::List([1, 2, 3].into_iter().map(DataValue::from).collect()),
            DataValue::List([2, 3, 4].into_iter().map(DataValue::from).collect()),
            DataValue::List([3, 4, 5].into_iter().map(DataValue::from).collect())
        ])
        .unwrap(),
        DataValue::List([1, 2, 3, 4, 5].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_intersection(&[
            DataValue::List(
                [1, 2, 3, 4, 5, 6]
                    .into_iter()
                    .map(DataValue::from)
                    .collect(),
            ),
            DataValue::List([2, 3, 4].into_iter().map(DataValue::from).collect()),
            DataValue::List([3, 4, 5].into_iter().map(DataValue::from).collect())
        ])
        .unwrap(),
        DataValue::List([3, 4].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_difference(&[
            DataValue::List(
                [1, 2, 3, 4, 5, 6]
                    .into_iter()
                    .map(DataValue::from)
                    .collect(),
            ),
            DataValue::List([2, 3, 4].into_iter().map(DataValue::from).collect()),
            DataValue::List([3, 4, 5].into_iter().map(DataValue::from).collect())
        ])
        .unwrap(),
        DataValue::List([1, 6].into_iter().map(DataValue::from).collect())
    );
}

#[test]
fn test_uuid() {
    let v1 = op_rand_uuid_v1(&[]).unwrap();
    let v4 = op_rand_uuid_v4(&[]).unwrap();
    assert!(op_is_uuid(&[v4]).unwrap().get_bool().unwrap());
    assert!(op_uuid_timestamp(&[v1]).unwrap().get_float().is_some());
    assert!(op_to_uuid(&[DataValue::from("")]).is_err());
    assert!(op_to_uuid(&[DataValue::from("f3b4958c-52a1-11e7-802a-010203040506")]).is_ok());
}

#[test]
fn test_now() {
    let now = op_now(&[]).unwrap();
    assert!(matches!(now, DataValue::Num(_)));
    let s = op_format_timestamp(&[now]).unwrap();
    let _dt = op_parse_timestamp(&[s]).unwrap();
}

#[test]
fn test_to_bool() {
    assert_eq!(
        op_to_bool(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(true)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(false)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(0.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from("")]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from("a")]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::List(vec![DataValue::from(0)])]).unwrap(),
        DataValue::from(true)
    );
}

#[test]
fn test_coalesce() {
    let db = DbInstance::default();
    let res = db.run_default("?[a] := a = null ~ 1 ~ 2").unwrap().rows;
    assert_eq!(res[0][0], DataValue::from(1));
    let res = db
        .run_default("?[a] := a = null ~ null ~ null")
        .unwrap()
        .rows;
    assert_eq!(res[0][0], DataValue::Null);
    let res = db.run_default("?[a] := a = 2 ~ null ~ 1").unwrap().rows;
    assert_eq!(res[0][0], DataValue::from(2));
}

#[test]
fn test_range() {
    let db = DbInstance::default();
    let res = db
        .run_default("?[a] := a = int_range(1, 5)")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0][0], json!([1, 2, 3, 4]));
    let res = db
        .run_default("?[a] := a = int_range(5)")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0][0], json!([0, 1, 2, 3, 4]));
    let res = db
        .run_default("?[a] := a = int_range(15, 3, -2)")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0][0], json!([15, 13, 11, 9, 7, 5]));
}

// ---- mnestic fork (0.13.0): datetime function library ----

fn dt_secs(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DataValue {
    use chrono::TimeZone;
    DataValue::from(
        chrono::Utc
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .unwrap()
            .timestamp() as f64,
    )
}

#[test]
fn test_dt_components() {
    // 2024-01-01 is a Monday, day-of-year 1
    let ts = || dt_secs(2024, 1, 1, 22, 30, 45);
    assert_eq!(op_dt_year(&[ts()]).unwrap(), DataValue::from(2024));
    assert_eq!(op_dt_month(&[ts()]).unwrap(), DataValue::from(1));
    assert_eq!(op_dt_day(&[ts()]).unwrap(), DataValue::from(1));
    assert_eq!(op_dt_hour(&[ts()]).unwrap(), DataValue::from(22));
    assert_eq!(op_dt_minute(&[ts()]).unwrap(), DataValue::from(30));
    assert_eq!(op_dt_second(&[ts()]).unwrap(), DataValue::from(45));
    assert_eq!(op_dt_dow(&[ts()]).unwrap(), DataValue::from(1));
    assert_eq!(op_dt_doy(&[ts()]).unwrap(), DataValue::from(1));
    // ISO dow: Sunday = 7 (2024-01-07)
    assert_eq!(
        op_dt_dow(&[dt_secs(2024, 1, 7, 0, 0, 0)]).unwrap(),
        DataValue::from(7)
    );
    // leap year day-of-year: Dec 31 2024 is day 366
    assert_eq!(
        op_dt_doy(&[dt_secs(2024, 12, 31, 12, 0, 0)]).unwrap(),
        DataValue::from(366)
    );
    // errors, not panics
    assert!(op_dt_year(&[DataValue::from("nope")]).is_err());
    assert!(op_dt_year(&[DataValue::from(f64::NAN)]).is_err());
    assert!(op_dt_year(&[DataValue::from(f64::INFINITY)]).is_err());
}

#[test]
fn test_dt_components_tz() {
    // 2024-01-01T03:30:00Z is 2023-12-31 22:30 in New York (UTC-5)
    let ts = || dt_secs(2024, 1, 1, 3, 30, 0);
    let ny = || DataValue::from("America/New_York");
    assert_eq!(op_dt_year(&[ts(), ny()]).unwrap(), DataValue::from(2023));
    assert_eq!(op_dt_month(&[ts(), ny()]).unwrap(), DataValue::from(12));
    assert_eq!(op_dt_day(&[ts(), ny()]).unwrap(), DataValue::from(31));
    assert_eq!(op_dt_hour(&[ts(), ny()]).unwrap(), DataValue::from(22));
    // Sunday in NY, Monday in UTC
    assert_eq!(op_dt_dow(&[ts(), ny()]).unwrap(), DataValue::from(7));
    assert!(op_dt_year(&[ts(), DataValue::from("Not/AZone")]).is_err());
}

#[test]
fn test_dt_trunc_units() {
    // 2024-05-15T13:45:30Z, a Wednesday
    let ts = || dt_secs(2024, 5, 15, 13, 45, 30);
    let tr = |unit: &str| op_dt_trunc(&[ts(), DataValue::from(unit)]).unwrap();
    assert_eq!(tr("year"), dt_secs(2024, 1, 1, 0, 0, 0));
    assert_eq!(tr("quarter"), dt_secs(2024, 4, 1, 0, 0, 0));
    assert_eq!(tr("month"), dt_secs(2024, 5, 1, 0, 0, 0));
    // ISO week: Monday 2024-05-13
    assert_eq!(tr("week"), dt_secs(2024, 5, 13, 0, 0, 0));
    assert_eq!(tr("day"), dt_secs(2024, 5, 15, 0, 0, 0));
    assert_eq!(tr("hour"), dt_secs(2024, 5, 15, 13, 0, 0));
    assert_eq!(tr("minute"), dt_secs(2024, 5, 15, 13, 45, 0));
    assert_eq!(tr("second"), dt_secs(2024, 5, 15, 13, 45, 30));
    // week truncation on a Monday is the identity on the date
    assert_eq!(
        op_dt_trunc(&[dt_secs(2024, 5, 13, 8, 0, 0), DataValue::from("week")]).unwrap(),
        dt_secs(2024, 5, 13, 0, 0, 0)
    );
    assert!(op_dt_trunc(&[ts(), DataValue::from("fortnight")]).is_err());
    // tz-aware day truncation: 2024-01-01T03:30Z in New York is still 2023-12-31
    // locally; local midnight is 05:00Z
    assert_eq!(
        op_dt_trunc(&[
            dt_secs(2024, 1, 1, 3, 30, 0),
            DataValue::from("day"),
            DataValue::from("America/New_York")
        ])
        .unwrap(),
        dt_secs(2023, 12, 31, 5, 0, 0)
    );
}

#[test]
fn test_dt_trunc_dst_trichotomy() {
    use std::str::FromStr;

    use chrono::offset::LocalResult;
    use chrono::TimeZone;

    // America/Havana springs forward at MIDNIGHT (00:00 -> 01:00) and falls
    // back TO midnight (01:00 -> 00:00), so its local midnights actually
    // exercise the None and Ambiguous arms. Guard on the LocalResult variant,
    // not the dates, so a tzdb bump that moves the transitions fails loudly
    // here instead of silently testing nothing.
    let havana = chrono_tz::Tz::from_str("America/Havana").unwrap();

    let spring_midnight = chrono::NaiveDate::from_ymd_opt(2025, 3, 9)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    assert!(
        matches!(
            havana.from_local_datetime(&spring_midnight),
            LocalResult::None
        ),
        "tzdb changed: Havana 2025-03-09 00:00 is no longer a DST gap; pick a new fixture"
    );
    // Nonexistent local midnight resolves forward to the first valid local
    // time after the gap: 01:00 local = 05:00Z (Havana was UTC-5 pre-jump).
    assert_eq!(
        op_dt_trunc(&[
            dt_secs(2025, 3, 9, 16, 0, 0),
            DataValue::from("day"),
            DataValue::from("America/Havana")
        ])
        .unwrap(),
        dt_secs(2025, 3, 9, 5, 0, 0)
    );

    let fall_midnight = chrono::NaiveDate::from_ymd_opt(2025, 11, 2)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    assert!(
        matches!(
            havana.from_local_datetime(&fall_midnight),
            LocalResult::Ambiguous(..)
        ),
        "tzdb changed: Havana 2025-11-02 00:00 is no longer ambiguous; pick a new fixture"
    );
    // Ambiguous local midnight resolves to its EARLIEST occurrence:
    // 00:00 local at UTC-4 = 04:00Z.
    assert_eq!(
        op_dt_trunc(&[
            dt_secs(2025, 11, 2, 17, 0, 0),
            DataValue::from("day"),
            DataValue::from("America/Havana")
        ])
        .unwrap(),
        dt_secs(2025, 11, 2, 4, 0, 0)
    );

    // Southern-hemisphere gap: Santiago springs forward 2025-09-07 at midnight.
    let santiago = chrono_tz::Tz::from_str("America/Santiago").unwrap();
    let scl_midnight = chrono::NaiveDate::from_ymd_opt(2025, 9, 7)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    assert!(
        matches!(
            santiago.from_local_datetime(&scl_midnight),
            LocalResult::None
        ),
        "tzdb changed: Santiago 2025-09-07 00:00 is no longer a DST gap; pick a new fixture"
    );
    assert_eq!(
        op_dt_trunc(&[
            dt_secs(2025, 9, 7, 16, 0, 0),
            DataValue::from("day"),
            DataValue::from("America/Santiago")
        ])
        .unwrap(),
        // 01:00 local at the new UTC-3 offset = 04:00Z
        dt_secs(2025, 9, 7, 4, 0, 0)
    );
}

#[test]
fn test_dt_add_calendar() {
    let add = |ts: DataValue, n: i64, unit: &str| {
        op_dt_add(&[ts, DataValue::from(n), DataValue::from(unit)])
    };
    // month-end clamping
    assert_eq!(
        add(dt_secs(2024, 1, 31, 12, 0, 0), 1, "month").unwrap(),
        dt_secs(2024, 2, 29, 12, 0, 0)
    );
    // leap day + 1 year clamps to Feb 28
    assert_eq!(
        add(dt_secs(2024, 2, 29, 0, 0, 0), 1, "year").unwrap(),
        dt_secs(2025, 2, 28, 0, 0, 0)
    );
    // negative months clamp too
    assert_eq!(
        add(dt_secs(2024, 3, 31, 0, 0, 0), -1, "month").unwrap(),
        dt_secs(2024, 2, 29, 0, 0, 0)
    );
    assert_eq!(
        add(dt_secs(2024, 1, 1, 0, 0, 0), 2, "quarter").unwrap(),
        dt_secs(2024, 7, 1, 0, 0, 0)
    );
    assert_eq!(
        add(dt_secs(2024, 1, 1, 0, 0, 0), 2, "week").unwrap(),
        dt_secs(2024, 1, 15, 0, 0, 0)
    );
    // fixed-duration day arithmetic crosses the Feb 29 boundary
    assert_eq!(
        add(dt_secs(2024, 2, 28, 0, 0, 0), 2, "day").unwrap(),
        dt_secs(2024, 3, 1, 0, 0, 0)
    );
    assert_eq!(
        add(dt_secs(2024, 1, 1, 23, 0, 0), 2, "hour").unwrap(),
        dt_secs(2024, 1, 2, 1, 0, 0)
    );
    assert_eq!(
        add(dt_secs(2024, 1, 1, 0, 59, 30), 1, "minute").unwrap(),
        dt_secs(2024, 1, 1, 1, 0, 30)
    );
    assert_eq!(
        add(dt_secs(2024, 1, 1, 0, 0, 0), -1, "second").unwrap(),
        dt_secs(2023, 12, 31, 23, 59, 59)
    );
    // errors, not panics
    assert!(add(dt_secs(2024, 1, 1, 0, 0, 0), i64::MAX, "day").is_err());
    assert!(add(dt_secs(2024, 1, 1, 0, 0, 0), i64::MAX, "year").is_err());
    assert!(add(dt_secs(2024, 1, 1, 0, 0, 0), 1, "fortnight").is_err());
}

#[test]
fn test_dt_diff() {
    let diff = |a: DataValue, b: DataValue, unit: &str| {
        op_dt_diff(&[a, b, DataValue::from(unit)]).unwrap()
    };
    // whole months, truncating: Jan 31 -> Feb 28 is NOT a full month...
    assert_eq!(
        diff(
            dt_secs(2024, 2, 28, 0, 0, 0),
            dt_secs(2024, 1, 31, 0, 0, 0),
            "month"
        ),
        DataValue::from(0)
    );
    // ...but Jan 31 -> Feb 29 IS, consistently with dt_add's clamping
    assert_eq!(
        diff(
            dt_secs(2024, 2, 29, 0, 0, 0),
            dt_secs(2024, 1, 31, 0, 0, 0),
            "month"
        ),
        DataValue::from(1)
    );
    assert_eq!(
        diff(
            dt_secs(2024, 3, 1, 0, 0, 0),
            dt_secs(2024, 1, 31, 0, 0, 0),
            "month"
        ),
        DataValue::from(1)
    );
    // sign symmetry (truncation toward zero)
    assert_eq!(
        diff(
            dt_secs(2024, 1, 31, 0, 0, 0),
            dt_secs(2024, 3, 1, 0, 0, 0),
            "month"
        ),
        DataValue::from(-1)
    );
    // year truncates: 11 months is 0 years
    assert_eq!(
        diff(
            dt_secs(2024, 12, 1, 0, 0, 0),
            dt_secs(2024, 1, 1, 0, 0, 0),
            "year"
        ),
        DataValue::from(0)
    );
    assert_eq!(
        diff(
            dt_secs(2025, 1, 1, 0, 0, 0),
            dt_secs(2024, 1, 1, 0, 0, 0),
            "year"
        ),
        DataValue::from(1)
    );
    assert_eq!(
        diff(
            dt_secs(2024, 7, 1, 0, 0, 0),
            dt_secs(2024, 1, 1, 0, 0, 0),
            "quarter"
        ),
        DataValue::from(2)
    );
    // fixed units truncate toward zero
    assert_eq!(
        diff(
            dt_secs(2024, 1, 2, 23, 59, 59),
            dt_secs(2024, 1, 1, 0, 0, 0),
            "day"
        ),
        DataValue::from(1)
    );
    assert_eq!(
        diff(
            dt_secs(2024, 1, 1, 1, 30, 0),
            dt_secs(2024, 1, 1, 0, 0, 0),
            "hour"
        ),
        DataValue::from(1)
    );
    assert_eq!(
        diff(
            dt_secs(2024, 1, 15, 0, 0, 0),
            dt_secs(2024, 1, 1, 0, 0, 0),
            "week"
        ),
        DataValue::from(2)
    );
    assert_eq!(
        diff(
            dt_secs(2024, 1, 1, 0, 1, 30),
            dt_secs(2024, 1, 1, 0, 0, 0),
            "second"
        ),
        DataValue::from(90)
    );
    assert!(op_dt_diff(&[
        dt_secs(2024, 1, 1, 0, 0, 0),
        dt_secs(2024, 1, 1, 0, 0, 0),
        DataValue::from("fortnight")
    ])
    .is_err());
}

#[test]
fn test_dt_format() {
    let ts = || dt_secs(2024, 5, 15, 13, 45, 30);
    assert_eq!(
        op_dt_format(&[ts(), DataValue::from("%Y-%m-%d %H:%M:%S")]).unwrap(),
        DataValue::from("2024-05-15 13:45:30")
    );
    assert_eq!(
        op_dt_format(&[
            ts(),
            DataValue::from("%Y-%m-%d %H:%M"),
            DataValue::from("America/New_York")
        ])
        .unwrap(),
        DataValue::from("2024-05-15 09:45")
    );
    // an invalid strftime specifier is a loud error, not a chrono panic
    assert!(op_dt_format(&[ts(), DataValue::from("%Q")]).is_err());
    assert!(op_dt_format(&[ts(), DataValue::from("100%")]).is_err());
    assert!(op_dt_format(&[ts(), DataValue::from(1)]).is_err());
}

#[test]
fn test_parse_timestamp_widened() {
    // RFC3339 (the original contract)
    assert_eq!(
        op_parse_timestamp(&[DataValue::from("2024-01-01T00:00:00Z")]).unwrap(),
        dt_secs(2024, 1, 1, 0, 0, 0)
    );
    assert_eq!(
        op_parse_timestamp(&[DataValue::from("2024-01-01T05:00:00+05:00")]).unwrap(),
        dt_secs(2024, 1, 1, 0, 0, 0)
    );
    // naive datetime, read as UTC
    assert_eq!(
        op_parse_timestamp(&[DataValue::from("2024-01-01 12:30:45")]).unwrap(),
        dt_secs(2024, 1, 1, 12, 30, 45)
    );
    // fractional seconds survive
    assert_eq!(
        op_parse_timestamp(&[DataValue::from("2024-01-01 12:30:45.5")]).unwrap(),
        DataValue::from(1704112245.5)
    );
    // bare date, midnight UTC
    assert_eq!(
        op_parse_timestamp(&[DataValue::from("2024-01-01")]).unwrap(),
        dt_secs(2024, 1, 1, 0, 0, 0)
    );
    // pre-epoch stays supported
    assert_eq!(
        op_parse_timestamp(&[DataValue::from("1969-07-20")]).unwrap(),
        dt_secs(1969, 7, 20, 0, 0, 0)
    );
    // nothing else parses, and the error enumerates the accepted forms
    for bad in [
        "01/01/2024",
        "2024-1-1T00:00",
        "yesterday",
        "2024-01-01T25:00:00Z",
    ] {
        let err = op_parse_timestamp(&[DataValue::from(bad)]).unwrap_err();
        assert!(err.to_string().contains("accepted forms"), "{bad}: {err}");
    }
}

#[test]
fn test_dt_to_validity() {
    use std::cmp::Reverse;

    use crate::data::value::Validity;

    // seconds -> microseconds, inside the function where the unit is known
    let v = op_dt_to_validity(&[dt_secs(2024, 1, 1, 0, 0, 0)]).unwrap();
    match &v {
        DataValue::Validity(Validity {
            timestamp,
            is_assert,
        }) => {
            assert_eq!(timestamp.0 .0, 1_704_067_200_000_000);
            assert_eq!(*is_assert, Reverse(true));
        }
        _ => panic!("expected a Validity, got {v:?}"),
    }
    // the documented round-trip: to_int(dt_to_validity(s)) == s * 1_000_000
    assert_eq!(
        op_to_int(&[v]).unwrap(),
        DataValue::from(1_704_067_200_000_000i64)
    );
    // retraction flag
    let v = op_dt_to_validity(&[dt_secs(2024, 1, 1, 0, 0, 0), DataValue::from(false)]).unwrap();
    match &v {
        DataValue::Validity(Validity { is_assert, .. }) => {
            assert_eq!(*is_assert, Reverse(false))
        }
        _ => panic!("expected a Validity, got {v:?}"),
    }
    // pre-epoch is a supported validity range
    let v = op_dt_to_validity(&[dt_secs(1969, 7, 20, 0, 0, 0)]).unwrap();
    match &v {
        DataValue::Validity(Validity { timestamp, .. }) => {
            assert!(timestamp.0 .0 < 0);
        }
        _ => panic!("expected a Validity, got {v:?}"),
    }
    // errors, not silent misreads
    assert!(op_dt_to_validity(&[DataValue::from(f64::NAN)]).is_err());
    assert!(op_dt_to_validity(&[DataValue::from("2024-01-01")]).is_err());
    assert!(op_dt_to_validity(&[dt_secs(2024, 1, 1, 0, 0, 0), DataValue::from(1)]).is_err());
}

#[test]
fn test_dt_validity_bridge_composes_with_time_travel() {
    // The positive test that the typed path is the WORKING path: before the
    // `@ <Validity>` arm, `@ dt_to_validity(...)` could not even be spelled
    // (`expr2vld_spec` bailed on DataValue::Validity).
    let db = DbInstance::default();
    db.run_default(":create facts {k: Int, at: Validity => x: Int}")
        .unwrap();
    // asserted 2024-01-01T00:00:00Z, retracted 2024-06-01T00:00:00Z
    db.run_default(
        "?[k, at, x] <- [[1, [1704067200000000, true], 7], [1, [1717200000000000, false], 7]] \
         :put facts {k, at => x}",
    )
    .unwrap();
    // as-of 2024-03-01 via the typed bridge: the row is live
    let res = db
        .run_default(
            "?[x] := *facts[1, at, x @ dt_to_validity(parse_timestamp('2024-03-01T00:00:00Z'))]",
        )
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[7]]));
    // as-of 2023 (before assertion): absent
    let res = db
        .run_default("?[x] := *facts[1, at, x @ dt_to_validity(parse_timestamp('2023-12-31'))]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([]));
    // as-of 2024-07 (after retraction): absent
    let res = db
        .run_default("?[x] := *facts[1, at, x @ dt_to_validity(parse_timestamp('2024-07-01'))]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([]));
    // the raw float is still the loud error 0.12.2 made it (the bridge is the
    // only way through, and it must stay that way)
    assert!(db
        .run_default("?[x] := *facts[1, at, x @ parse_timestamp('2024-03-01T00:00:00Z')]")
        .is_err());
}

#[test]
fn test_dt_validity_bridge_on_tt_axis() {
    let db = DbInstance::default();
    db.run_default(":create audit_dt {k: Int, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put audit_dt {k => v}")
        .unwrap();
    // :as_of far in the future via the typed bridge sees the row
    let res = db
        .run_default(
            "?[v] := *audit_dt[k, t, v] \
             :as_of dt_to_validity(parse_timestamp('2990-01-01T00:00:00Z'))",
        )
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[10]]));
    // :as_of before the commit sees nothing
    let res = db
        .run_default(
            "?[v] := *audit_dt[k, t, v] \
             :as_of dt_to_validity(parse_timestamp('1990-01-01T00:00:00Z'))",
        )
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([]));
}

// ---- 0.13.0 review fixes: regression tests ----

/// Sub-day truncation inside a DST fall-back fold must stay in the fold arm
/// the instant belongs to (the input's own offset disambiguates); it must be
/// idempotent and containing. Pre-fix, the earliest-occurrence policy returned
/// results a full hour early for second-pass instants.
#[test]
fn test_dt_trunc_dst_fold_prefers_input_offset() {
    use std::str::FromStr;

    use chrono::offset::LocalResult;
    use chrono::TimeZone;

    let ny = chrono_tz::Tz::from_str("America/New_York").unwrap();
    let fold = chrono::NaiveDate::from_ymd_opt(2025, 11, 2)
        .unwrap()
        .and_hms_opt(1, 0, 0)
        .unwrap();
    assert!(
        matches!(ny.from_local_datetime(&fold), LocalResult::Ambiguous(..)),
        "tzdb changed: NY 2025-11-02 01:00 is no longer ambiguous; pick a new fixture"
    );
    let tr = |ts: DataValue, unit: &str| {
        op_dt_trunc(&[ts, DataValue::from(unit), DataValue::from("America/New_York")]).unwrap()
    };
    // 06:30Z = 01:30 EST — the SECOND pass of the fold.
    assert_eq!(
        tr(dt_secs(2025, 11, 2, 6, 30, 0), "hour"),
        dt_secs(2025, 11, 2, 6, 0, 0),
        "hour start must be in the same (EST) arm, not an hour early in EDT"
    );
    assert_eq!(
        tr(dt_secs(2025, 11, 2, 6, 30, 0), "minute"),
        dt_secs(2025, 11, 2, 6, 30, 0)
    );
    // truncation to the second of an on-boundary instant is the identity
    assert_eq!(
        tr(dt_secs(2025, 11, 2, 6, 30, 0), "second"),
        dt_secs(2025, 11, 2, 6, 30, 0)
    );
    // 05:30Z = 01:30 EDT — the FIRST pass stays in EDT.
    assert_eq!(
        tr(dt_secs(2025, 11, 2, 5, 30, 0), "hour"),
        dt_secs(2025, 11, 2, 5, 0, 0)
    );
}

/// The admitted timestamp range's edges must produce loud errors, not chrono
/// panics (pre-fix: `NaiveDate - TimeDelta` overflow in the 'week' arm, and
/// `date_naive()`'s offset-overflow expect with non-UTC zones).
#[test]
fn test_dt_trunc_range_edges_error_not_panic() {
    // first partial week above NaiveDate::MIN (a Thursday): Monday underflows
    assert!(op_dt_trunc(&[
        DataValue::from(-8334601227800.001),
        DataValue::from("week")
    ])
    .is_err());
    // MIN end + negative-offset zone: local date below NaiveDate::MIN
    assert!(op_dt_trunc(&[
        DataValue::from(-8334601228799.0),
        DataValue::from("day"),
        DataValue::from("Etc/GMT+12")
    ])
    .is_err());
    // MAX end + positive-offset zone: local date above NaiveDate::MAX
    assert!(op_dt_trunc(&[
        DataValue::from(8210298412799.0),
        DataValue::from("day"),
        DataValue::from("Pacific/Kiritimati")
    ])
    .is_err());
}

/// Historic transitions deeper than 4 h (the Antarctic station foundings jump
/// 6–10 h at exactly local midnight) must resolve, not hard-error: the probe
/// ladder now extends hourly to 26 h. Guarded on the LocalResult so a tzdb
/// bump that moves the transition fails loudly here.
#[test]
fn test_dt_trunc_resolves_deep_historic_gaps() {
    use std::str::FromStr;

    use chrono::offset::LocalResult;
    use chrono::{Datelike, TimeZone, Timelike};

    let casey = chrono_tz::Tz::from_str("Antarctica/Casey").unwrap();
    let midnight = chrono::NaiveDate::from_ymd_opt(1969, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    assert!(
        matches!(casey.from_local_datetime(&midnight), LocalResult::None),
        "tzdb changed: Casey 1969-01-01 00:00 is no longer a gap; pick a new fixture"
    );
    // ...and the gap is deeper than the old 4 h ladder (the discriminating
    // premise: pre-fix this test errored).
    let four_h = midnight + chrono::Duration::hours(4);
    assert!(
        matches!(casey.from_local_datetime(&four_h), LocalResult::None),
        "tzdb changed: the Casey founding gap is no longer > 4 h"
    );
    let out = op_dt_trunc(&[
        dt_secs(1969, 6, 1, 0, 0, 0),
        DataValue::from("year"),
        DataValue::from("Antarctica/Casey"),
    ])
    .unwrap();
    // The resolved instant is the first valid local time after the gap.
    let secs = out.get_float().unwrap();
    let local = chrono::Utc
        .timestamp_micros((secs * 1_000_000.).round() as i64)
        .unwrap()
        .with_timezone(&casey);
    assert_eq!(
        (local.year(), local.month(), local.day(), local.minute()),
        (1969, 1, 1, 0),
        "resolved instant must be the gap end on the founding day: {local:?}"
    );
    assert!(
        matches!(
            casey.from_local_datetime(&local.naive_local()),
            LocalResult::Single(_)
        ),
        "resolved local time must actually exist: {local:?}"
    );
}

/// The i64 extremes are reserved engine sentinels: dt_to_validity's one-ulp
/// guard gap must not mint MAX_VALIDITY_TS ('NOW'/'END') or TERMINAL_VALIDITY
/// (whose key livelocks temporal scans), and validity() must reject them too.
// The over-precise float literals are the point: they pin the exact f64 whose
// `(f * 1e6).round()` lands on ±2^63 — truncating them would test a
// different value.
#[allow(clippy::excessive_precision)]
#[test]
fn test_validity_constructors_reject_sentinels() {
    // (f * 1e6).round() == 2^63 exactly for this literal — pre-fix it passed
    // the `> i64::MAX as f64` guard and saturated to MAX_VALIDITY_TS.
    assert!(op_dt_to_validity(&[DataValue::from(9223372036854.7754)]).is_err());
    assert!(op_dt_to_validity(&[
        DataValue::from(-9223372036854.775808),
        DataValue::from(false)
    ])
    .is_err());
    assert!(op_validity(&[DataValue::from(i64::MAX)]).is_err());
    assert!(op_validity(&[DataValue::from(i64::MIN), DataValue::from(false)]).is_err());
    // near-but-legal values still construct
    assert!(op_dt_to_validity(&[DataValue::from(9223372036000.0)]).is_ok());
    assert!(op_validity(&[DataValue::from(i64::MAX - 1)]).is_ok());
    assert!(op_validity(&[DataValue::from(i64::MIN + 1), DataValue::from(false)]).is_ok());
}

/// One datetime string grammar for both entry points: a literal parse_timestamp
/// accepts must be accepted by validity string literals (`@ '...'`) too.
#[test]
fn test_validity_literals_share_parse_timestamp_grammar() {
    assert_eq!(
        str2vld("2024-06-01 12:00:00").unwrap(),
        str2vld("2024-06-01T12:00:00Z").unwrap()
    );
    let db = DbInstance::default();
    db.run_default(":create gf {k: Int, at: Validity => x: Int}")
        .unwrap();
    db.run_default("?[k, at, x] <- [[1, [1717243200000000, true], 5]] :put gf {k, at => x}")
        .unwrap();
    let res = db
        .run_default("?[x] := *gf[1, at, x @ '2024-06-01 12:00:00']")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[5]]));
}

/// Negative-span dt_diff semantics, pinned: antisymmetric truncation toward
/// zero — deliberately NOT the floor form (month-end clamping is asymmetric;
/// see the op_dt_diff comment).
#[test]
fn test_dt_diff_negative_span_is_antisymmetric() {
    let diff = |a: DataValue, b: DataValue| {
        op_dt_diff(&[a, b, DataValue::from("month")]).unwrap()
    };
    assert_eq!(
        diff(dt_secs(2023, 1, 30, 0, 0, 0), dt_secs(2023, 3, 31, 0, 0, 0)),
        DataValue::from(-2),
        "two whole months lie between them; the floor form's -3 is rejected by design"
    );
    assert_eq!(
        diff(dt_secs(2023, 3, 31, 0, 0, 0), dt_secs(2023, 1, 30, 0, 0, 0)),
        DataValue::from(2)
    );
}
