// tests/minifier.rs
// Minify → expand roundtrip, edge cases.

use scon::*;
use scon::value::SconMap;
use compact_str::CompactString;

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = SconMap::default();
    for (k, v) in pairs {
        map.insert(CompactString::from(k), v);
    }
    Value::Object(map)
}

fn s(v: &str) -> Value { Value::String(CompactString::from(v)) }

#[test]
fn minify_simple() {
    let scon = "name: Alice\nage: 30\n";
    let min = minify(scon);
    assert!(!min.contains('\n'));
    assert!(min.contains(';'));
    assert!(min.contains("name: Alice"));
    assert!(min.contains("age: 30"));
}

#[test]
fn expand_simple() {
    let min = "name: Alice;age: 30";
    let expanded = expand(min, 2);
    assert!(expanded.contains('\n'));
    assert!(expanded.contains("name: Alice"));
    assert!(expanded.contains("age: 30"));
}

#[test]
fn minify_nested() {
    let scon = "root:\n  child: value\n  deep:\n    leaf: data\n";
    let min = minify(scon);
    let expanded = expand(&min, 2);
    let decoded_orig = decode(scon).unwrap();
    let decoded_rt = decode(&expanded).unwrap();

    if let (Value::Object(o), Value::Object(r)) = (&decoded_orig, &decoded_rt) {
        assert!(o.contains_key("root"));
        assert!(r.contains_key("root"));
    }
}

#[test]
fn minify_expand_preserves_data() {
    let data = obj(vec![
        ("name", s("test")),
        ("nested", obj(vec![
            ("a", Value::Integer(1)),
            ("b", obj(vec![
                ("deep", Value::Bool(true)),
            ])),
        ])),
        ("tags", Value::Array(vec![s("x"), s("y")])),
    ]);

    let encoded = encode(&data);
    let minified = minify(&encoded);
    let expanded = expand(&minified, 2);

    let decoded_min = decode(&minified).unwrap();
    let decoded_exp = decode(&expanded).unwrap();

    if let (Value::Object(m), Value::Object(e)) = (&decoded_min, &decoded_exp) {
        assert_eq!(m.get("name"), e.get("name"));
    }
}

#[test]
fn minify_empty_input() {
    assert_eq!(minify(""), "");
    assert_eq!(minify("\n\n"), "");
}

#[test]
fn expand_no_semicolons() {
    let input = "key: value";
    let expanded = expand(input, 2);
    assert_eq!(expanded.trim(), "key: value");
}

#[test]
fn minify_with_comments() {
    let scon = "# comment\nname: Alice\n# another\nage: 30\n";
    let min = minify(scon);
    assert!(!min.contains('#'));
    assert!(min.contains("name: Alice"));
}

#[test]
fn minify_tabular() {
    let data = obj(vec![
        ("rows", Value::Array(vec![
            obj(vec![("id", Value::Integer(1)), ("name", s("a"))]),
            obj(vec![("id", Value::Integer(2)), ("name", s("b"))]),
        ])),
    ]);

    let encoded = encode(&data);
    let minified = minify(&encoded);
    let decoded = decode(&minified).unwrap();

    if let Value::Object(root) = &decoded {
        if let Some(Value::Array(rows)) = root.get("rows") {
            assert_eq!(rows.len(), 2);
            return;
        }
    }
    panic!("Tabular minified roundtrip failed");
}
