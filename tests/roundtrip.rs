// tests/roundtrip.rs
// Encode → decode roundtrip for all Value types, tabular, nested, minified.

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
fn primitives() {
    let data = obj(vec![
        ("null_val", Value::Null),
        ("bool_true", Value::Bool(true)),
        ("bool_false", Value::Bool(false)),
        ("int_pos", Value::Integer(42)),
        ("int_neg", Value::Integer(-100)),
        ("float_val", Value::Float(3.14)),
        ("str_simple", s("hello")),
        ("str_empty", s("")),
        ("str_special", s("has spaces & stuff")),
    ]);

    let encoded = encode(&data);
    let decoded = decode(&encoded).unwrap();

    if let (Value::Object(orig), Value::Object(dec)) = (&data, &decoded) {
        assert_eq!(orig.len(), dec.len());
        assert_eq!(dec.get("null_val"), Some(&Value::Null));
        assert_eq!(dec.get("bool_true"), Some(&Value::Bool(true)));
        assert_eq!(dec.get("bool_false"), Some(&Value::Bool(false)));
        assert_eq!(dec.get("int_pos"), Some(&Value::Integer(42)));
        assert_eq!(dec.get("int_neg"), Some(&Value::Integer(-100)));
        assert_eq!(dec.get("str_simple").and_then(|v| v.as_str()), Some("hello"));
    } else {
        panic!("Expected objects");
    }
}

#[test]
fn nested_objects() {
    let data = obj(vec![
        ("level1", obj(vec![
            ("level2", obj(vec![
                ("value", s("deep")),
            ])),
        ])),
    ]);

    let encoded = encode(&data);
    let decoded = decode(&encoded).unwrap();

    if let Value::Object(root) = &decoded {
        if let Some(Value::Object(l1)) = root.get("level1") {
            if let Some(Value::Object(l2)) = l1.get("level2") {
                assert_eq!(l2.get("value").and_then(|v| v.as_str()), Some("deep"));
                return;
            }
        }
    }
    panic!("Nested structure not preserved");
}

#[test]
fn inline_array() {
    let data = obj(vec![
        ("tags", Value::Array(vec![s("a"), s("b"), s("c")])),
    ]);

    let encoded = encode(&data);
    let decoded = decode(&encoded).unwrap();

    if let Value::Object(root) = &decoded {
        if let Some(Value::Array(arr)) = root.get("tags") {
            assert_eq!(arr.len(), 3);
            assert_eq!(arr[0].as_str(), Some("a"));
            assert_eq!(arr[2].as_str(), Some("c"));
            return;
        }
    }
    panic!("Inline array not preserved");
}

#[test]
fn tabular_array() {
    let rows: Vec<Value> = (0..3).map(|i| {
        obj(vec![
            ("id", Value::Integer(i)),
            ("name", s(&format!("item_{}", i))),
        ])
    }).collect();
    let data = obj(vec![("items", Value::Array(rows))]);

    let encoded = encode(&data);
    let decoded = decode(&encoded).unwrap();

    if let Value::Object(root) = &decoded {
        if let Some(Value::Array(arr)) = root.get("items") {
            assert_eq!(arr.len(), 3);
            if let Value::Object(row) = &arr[1] {
                assert_eq!(row.get("id"), Some(&Value::Integer(1)));
                assert_eq!(row.get("name").and_then(|v| v.as_str()), Some("item_1"));
                return;
            }
        }
    }
    panic!("Tabular array not preserved");
}

#[test]
fn expanded_list_with_objects() {
    let items = Value::Array(vec![
        obj(vec![("key", s("first")), ("val", Value::Integer(1))]),
        obj(vec![("key", s("second")), ("val", Value::Integer(2))]),
    ]);
    let data = obj(vec![("entries", items)]);

    let encoded = encode(&data);
    let decoded = decode(&encoded).unwrap();

    if let Value::Object(root) = &decoded {
        if let Some(Value::Array(arr)) = root.get("entries") {
            assert_eq!(arr.len(), 2);
            return;
        }
    }
    panic!("Expanded list not preserved");
}

#[test]
fn minify_expand_roundtrip() {
    let data = obj(vec![
        ("name", s("test")),
        ("nested", obj(vec![
            ("child", Value::Integer(42)),
        ])),
    ]);

    let encoded = encode(&data);
    let minified = minify(&encoded);
    assert!(!minified.contains('\n'));
    assert!(minified.contains(';'));

    let expanded = expand(&minified, 2);
    assert!(expanded.contains('\n'));

    let decoded_min = decode(&minified).unwrap();
    let decoded_exp = decode(&expanded).unwrap();

    // Both should produce same structure
    if let (Value::Object(m), Value::Object(e)) = (&decoded_min, &decoded_exp) {
        assert_eq!(m.get("name").and_then(|v| v.as_str()), Some("test"));
        assert_eq!(e.get("name").and_then(|v| v.as_str()), Some("test"));
    } else {
        panic!("Minified roundtrip failed");
    }
}

#[test]
fn empty_structures() {
    let data = obj(vec![
        ("empty_obj", Value::Object(SconMap::default())),
        ("empty_arr", Value::Array(vec![])),
    ]);

    let encoded = encode(&data);
    let decoded = decode(&encoded).unwrap();

    if let Value::Object(root) = &decoded {
        assert!(matches!(root.get("empty_obj"), Some(Value::Object(m)) if m.is_empty()));
        assert!(matches!(root.get("empty_arr"), Some(Value::Array(a)) if a.is_empty()));
    } else {
        panic!("Empty structures not preserved");
    }
}

#[test]
fn custom_indent() {
    let data = obj(vec![
        ("a", obj(vec![("b", Value::Integer(1))])),
    ]);

    let encoded_2 = encode_with_indent(&data, 2);
    let encoded_4 = encode_with_indent(&data, 4);

    assert!(encoded_2.contains("  b:"));
    assert!(encoded_4.contains("    b:"));

    let d2 = decode(&encoded_2).unwrap();
    let d4 = decode(&encoded_4).unwrap();

    if let (Value::Object(r2), Value::Object(r4)) = (&d2, &d4) {
        assert!(r2.contains_key("a"));
        assert!(r4.contains_key("a"));
    }
}
