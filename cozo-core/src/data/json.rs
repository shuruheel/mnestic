/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::json;
pub(crate) use serde_json::Value as JsonValue;

use crate::data::value::{DataValue, Num, Vector};
use crate::JsonData;

impl From<JsonValue> for DataValue {
    fn from(v: JsonValue) -> Self {
        match v {
            JsonValue::Null => DataValue::Null,
            JsonValue::Bool(b) => DataValue::Bool(b),
            JsonValue::Number(n) => match n.as_i64() {
                Some(i) => DataValue::from(i),
                None => match n.as_f64() {
                    Some(f) => DataValue::from(f),
                    None => DataValue::from(n.to_string()),
                },
            },
            JsonValue::String(s) => DataValue::from(s),
            JsonValue::Array(arr) => DataValue::List(arr.iter().map(DataValue::from).collect()),
            JsonValue::Object(d) => DataValue::Json(JsonData(JsonValue::Object(d))),
        }
    }
}

impl<'a> From<&'a JsonValue> for DataValue {
    fn from(v: &'a JsonValue) -> Self {
        match v {
            JsonValue::Null => DataValue::Null,
            JsonValue::Bool(b) => DataValue::Bool(*b),
            JsonValue::Number(n) => match n.as_i64() {
                Some(i) => DataValue::from(i),
                None => match n.as_f64() {
                    Some(f) => DataValue::from(f),
                    None => DataValue::from(n.to_string()),
                },
            },
            JsonValue::String(s) => DataValue::Str(s.into()),
            JsonValue::Array(arr) => DataValue::List(arr.iter().map(DataValue::from).collect()),
            JsonValue::Object(d) => DataValue::Json(JsonData(JsonValue::Object(d.clone()))),
        }
    }
}

// The single outbound JSON policy, shared by result cells and nested values.
impl From<&DataValue> for JsonValue {
    fn from(v: &DataValue) -> Self {
        match v {
            DataValue::Null | DataValue::Bot => JsonValue::Null,
            DataValue::Bool(b) => JsonValue::Bool(*b),
            DataValue::Num(Num::Int(i)) => JsonValue::Number((*i).into()),
            DataValue::Num(Num::Float(f)) => {
                if f.is_infinite() {
                    json!(if f.is_sign_negative() {
                        "NEGATIVE_INFINITY"
                    } else {
                        "INFINITY"
                    })
                } else {
                    json!(f)
                }
            }
            DataValue::Str(t) => JsonValue::String(t.to_string()),
            DataValue::Bytes(bytes) => JsonValue::String(STANDARD.encode(bytes)),
            DataValue::List(l) => JsonValue::Array(l.iter().map(JsonValue::from).collect()),
            DataValue::Set(l) => JsonValue::Array(l.iter().map(JsonValue::from).collect()),
            DataValue::Regex(r) => json!(r.0.as_str()),
            DataValue::Uuid(u) => json!(u.0),
            DataValue::Vec(arr) => {
                // Preserve F32 widening and non-finite -> null, including
                // arrays whose logical elements are not contiguous in memory.
                let number = |f: f64| {
                    serde_json::Number::from_f64(f).map_or(JsonValue::Null, JsonValue::Number)
                };
                JsonValue::Array(match arr {
                    Vector::F32(a) => a.iter().map(|f| number(*f as f64)).collect(),
                    Vector::F64(a) => a.iter().map(|f| number(*f)).collect(),
                })
            }
            DataValue::Validity(v) => json!([v.timestamp.0, v.is_assert.0]),
            DataValue::Json(j) => j.0.clone(),
        }
    }
}

impl From<DataValue> for JsonValue {
    fn from(v: DataValue) -> Self {
        match v {
            // Move opaque JSON result cells without cloning their payload.
            DataValue::Json(j) => j.0,
            other => JsonValue::from(&other),
        }
    }
}
