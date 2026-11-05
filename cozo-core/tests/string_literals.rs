/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
use cozo::{DataValue, DbInstance, ScriptMutability};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn db() -> DbInstance {
    DbInstance::new("mem", "", "").unwrap()
}
fn run(db: &DbInstance, script: &str) -> Value {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}: {e:?}"))
        .into_json()["rows"]
        .clone()
}
fn literal(lit: &str, expected: &str) {
    assert_eq!(
        run(&db(), &format!("?[x] <- [[{lit}]]")),
        json!([[expected]]),
        "{lit}"
    );
}

#[test]
fn literals_preserve_content_and_decode_each_escape() {
    for (lit, expected) in [
        (r#""blah\"\"""#, "blah\"\""),
        (r###"___"#298594"___"###, "#298594"),
        (r#"" a ""#, " a "),
        (r#"__" a "__"#, " a "),
        (r#""/* x */a""#, "/* x */a"),
        (r#"_"a /* c */ b # d"_"#, "a /* c */ b # d"),
        ("_\"a # x\n b\"_", "a # x\n b"),
        ("_\"a\n b # x\"_", "a\n b # x"),
        ("_\"a\r\nb\"_", "a\r\nb"),
        ("\"a\r\nb\"", "a\r\nb"),
        ("\"x#\ny\\tz\"", "x#\ny\tz"),
        (r#"" a#b ""#, " a#b "),
        (r#""""#, ""),
        ("''", ""),
        (r#"___""___"#, ""),
        (r#"__"a"_"__"#, "a\"_"),
        (r#"__"a"_b"__"#, "a\"_b"),
        (r#""a'b""#, "a'b"),
        ("'a\"b'", "a\"b"),
        (r#""😀""#, "😀"),
        ("'😀'", "😀"),
        (r#"_"😀"_"#, "😀"),
        (r#"_"a\nb"_"#, r"a\nb"),
    ] {
        literal(lit, expected);
    }
    for (escaped, decoded) in [
        (r"\\", "\\"),
        (r"\/", "/"),
        (r"\b", "\x08"),
        (r"\f", "\x0c"),
        (r"\n", "\n"),
        (r"\r", "\r"),
        (r"\t", "\t"),
        (r"\u0041", "A"),
        (r"\u00e9", "é"),
    ] {
        literal(&format!("\"{escaped}\""), decoded);
        literal(&format!("'{escaped}'"), decoded);
    }
    literal(r#""\"""#, "\"");
    literal(r"'\''", "'");
}

#[test]
fn malformed_literals_are_loud() {
    for lit in [
        r#""a\qb""#,
        r#""C:\Users""#,
        r#"__ "abc" __"#,
        r#"__"a"___"#,
        r#"_"a""#,
        r#""a"_"#,
        r#"x_"a"_"#,
        r#"_x_"a"_"#,
    ] {
        let e = db()
            .run_script(
                &format!("?[x] <- [[{lit}]]"),
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(format!("{e:?}").contains("parser::pest"), "{lit}: {e:?}");
    }
    for quote in ['\'', '"'] {
        for escaped in [r"\uD800", r"\uDC00", r"\uD83D\uDE00"] {
            let e = db()
                .run_script(
                    &format!("?[x] <- [[{quote}{escaped}{quote}]]"),
                    BTreeMap::new(),
                    ScriptMutability::Mutable,
                )
                .unwrap_err();
            assert!(format!("{e:?}").contains("parser::invalid_utf8_code"));
        }
    }
    // Public JSON error spans are byte offsets, including multibyte prefixes.
    for script in [r#"?[x] <- [["a\qb"]]"#, "?[x] <- [[\"é\\qb\"]]"] {
        let result: Value =
            serde_json::from_str(&db().run_script_str(script, "{}", false)).unwrap();
        assert_eq!(result["code"], "parser::pest");
        assert_eq!(
            result["labels"][0]["span"]["offset"],
            script.find('\\').unwrap() + 1
        );
    }
    let script = r#"?[x] <- [["a\"]]"#;
    let result: Value = serde_json::from_str(&db().run_script_str(script, "{}", false)).unwrap();
    assert!(result["display"].as_str().unwrap().contains(&format!(
        "{}..{}",
        script.len(),
        script.len()
    )));
}

#[test]
fn comments_parameters_and_descriptions() {
    let d = db();
    assert_eq!(
        run(
            &d,
            "# before\n?[x,y] <- [[\"a\" /* between */, 'b']] # tail"
        ),
        json!([["a", "b"]])
    );
    assert_eq!(
        run(&d, r#"?[x] := x = "a /* " ++ " */ b""#),
        json!([["a /*  */ b"]])
    );
    let text = " \"#\\uD800\n😀 ";
    let result = d
        .run_script(
            "?[x] <- [[$x]]",
            BTreeMap::from([("x".into(), DataValue::from(text))]),
            ScriptMutability::Mutable,
        )
        .unwrap();
    assert_eq!(result.into_json()["rows"], json!([[text]]));
    run(&d, ":create t {x: String}");
    for (lit, expected) in [(r#""l1\nl2""#, "l1\nl2"), (r#"_"x # y"_"#, "x # y")] {
        run(&d, &format!("::describe t {lit}"));
        let rows = d
            .run_script("::relations", BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap();
        let col = rows
            .headers
            .iter()
            .position(|h| h == "description")
            .unwrap();
        assert!(rows
            .rows
            .iter()
            .any(|row| row[col] == DataValue::from(expected)));
    }
}

#[test]
fn migration_warning_is_retired_without_changing_errors() {
    let d = db();
    run(&d, r#"?[x] <- [["a\nb"], [_"  raw  "_]]"#);
    assert_eq!(run(&d, "::warnings"), json!([]));
    assert!(d
        .run_script(
            r#"?[x] <- [["\uD800"]]"#,
            BTreeMap::new(),
            ScriptMutability::Mutable
        )
        .is_err());
    assert_eq!(run(&d, "::warnings"), json!([]));
}

fn single_quote(s: &str) -> String {
    let body = serde_json::to_string(s).unwrap();
    // JSON's escaped double quote becomes plain; single quotes need escaping.
    let mut out = String::from("'");
    let mut chars = body[1..body.len() - 1].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let next = chars.next().unwrap();
                if next != '"' {
                    out.push('\\');
                }
                out.push(next);
            }
            '\'' => out.push_str("\\'"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}
fn raw_quote(s: &str) -> String {
    let longest = s
        .split('"')
        .skip(1)
        .map(|part| part.chars().take_while(|c| *c == '_').count())
        .max()
        .unwrap_or(0);
    let fence = "_".repeat(longest + 1);
    format!("{fence}\"{s}\"{fence}")
}

#[test]
fn bounded_literal_round_trips() {
    // 30,941 exhaustive + 2,000 seeded strings, three forms, <= 1,000 rows/script.
    let alphabet: Vec<char> = "a\"'\\#_/* \nnu0".chars().collect();
    assert_eq!(alphabet.len(), 13);
    let mut strings = vec![String::new()];
    let mut level = vec![String::new()];
    for _ in 0..4 {
        level = level
            .iter()
            .flat_map(|s| alphabet.iter().map(move |c| format!("{s}{c}")))
            .collect();
        strings.extend(level.iter().cloned());
    }
    assert_eq!(strings.len(), 30_941);
    let mut extended = alphabet;
    extended.extend(['é', '😀', '\r']);
    let mut state = 0x5354_cafe_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..2000 {
        let len = 5 + next() as usize % 36;
        strings.push(
            (0..len)
                .map(|_| extended[next() as usize % extended.len()])
                .collect(),
        );
    }
    let d = db();
    for encode in [
        |s: &str| serde_json::to_string(s).unwrap(),
        single_quote,
        raw_quote,
    ] {
        for chunk in strings.chunks(1000) {
            let rows = chunk
                .iter()
                .enumerate()
                .map(|(i, s)| format!("[{i},{}]", encode(s)))
                .collect::<Vec<_>>()
                .join(",");
            let result = run(&d, &format!("?[id,x] <- [{rows}] :order id"));
            let expected: Vec<Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, s)| json!([i, s]))
                .collect();
            assert_eq!(result, json!(expected));
        }
    }
}
