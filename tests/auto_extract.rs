// tests/auto_extract.rs
// Encode with autoExtract, verify @s: refs in output, roundtrip.

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
fn auto_extract_produces_schema_defs() {
    // 3 identical schemas should be extracted
    let schema = obj(vec![
        ("type", s("string")),
        ("format", s("email")),
    ]);
    let data = obj(vec![
        ("field1", schema.clone()),
        ("field2", schema.clone()),
        ("field3", schema.clone()),
    ]);

    let encoded = Encoder::new().with_auto_extract(true).encode(&data);
    assert!(encoded.contains("s:"), "Output should contain schema definitions");
    assert!(encoded.contains("@s:"), "Output should contain schema references");
}

#[test]
fn auto_extract_roundtrip() {
    let schema = obj(vec![
        ("type", s("object")),
        ("required", Value::Bool(true)),
    ]);
    let data = obj(vec![
        ("a", schema.clone()),
        ("b", schema.clone()),
        ("c", obj(vec![("unique", Value::Integer(42))])),
    ]);

    let encoded = Encoder::new().with_auto_extract(true).encode(&data);
    let decoded = decode(&encoded).unwrap();

    // a and b should resolve back to the same schema
    if let Value::Object(root) = &decoded {
        if let (Some(Value::Object(a)), Some(Value::Object(b))) =
            (root.get("a"), root.get("b"))
        {
            assert_eq!(a.get("type").and_then(|v| v.as_str()), Some("object"));
            assert_eq!(b.get("type").and_then(|v| v.as_str()), Some("object"));
            assert_eq!(a.get("required"), Some(&Value::Bool(true)));
            return;
        }
    }
    panic!("Auto-extract roundtrip failed: schema refs not resolved back");
}

#[test]
fn auto_extract_unique_not_extracted() {
    // Unique objects (appearing once) should NOT be extracted
    let data = obj(vec![
        ("x", obj(vec![("a", Value::Integer(1))])),
        ("y", obj(vec![("b", Value::Integer(2))])),
    ]);

    let encoded = Encoder::new().with_auto_extract(true).encode(&data);
    assert!(!encoded.contains("s:"), "Unique objects should not be extracted");
}

#[test]
fn auto_extract_with_flat_repeated() {
    // Repeated schemas at the same level should be extracted
    let inner = obj(vec![("type", s("integer")), ("min", Value::Integer(0))]);
    let data = obj(vec![
        ("field1", inner.clone()),
        ("field2", inner.clone()),
        ("field3", inner.clone()),
    ]);

    let encoded = Encoder::new().with_auto_extract(true).encode(&data);
    let decoded = decode(&encoded).unwrap();

    if let Value::Object(root) = &decoded {
        for key in &["field1", "field2", "field3"] {
            if let Some(Value::Object(field)) = root.get(*key) {
                assert_eq!(field.get("type").and_then(|v| v.as_str()), Some("integer"));
                assert_eq!(field.get("min"), Some(&Value::Integer(0)));
            } else {
                panic!("Field {} not resolved correctly", key);
            }
        }
    } else {
        panic!("Auto-extract with flat repeated failed");
    }
}

#[test]
fn encode_with_dedup_convenience() {
    let schema = obj(vec![("x", Value::Integer(1)), ("y", Value::Integer(2))]);
    let data = obj(vec![
        ("a", schema.clone()),
        ("b", schema.clone()),
    ]);

    let encoded = encode_with_dedup(&data);
    let decoded = decode(&encoded).unwrap();

    if let Value::Object(root) = &decoded {
        assert_eq!(root.get("a"), root.get("b"));
    }
}
