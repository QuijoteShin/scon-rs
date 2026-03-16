// tests/treehash.rs
// TreeHash: fingerprint type safety, hash_tree dedup detection, equals, diff.

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
fn hash_deterministic() {
    let a = obj(vec![("x", Value::Integer(1)), ("y", s("hello"))]);
    let h1 = TreeHash::hash(&a);
    let h2 = TreeHash::hash(&a);
    assert_eq!(h1, h2, "Same input must produce same hash");
    assert_eq!(h1.len(), 32, "Hash must be 32-char hex");
}

#[test]
fn hash_type_safety() {
    // Integer 1 vs Float 1.0 — fingerprint distinguishes types
    let int_val = Value::Integer(1);
    let float_val = Value::Float(1.0);
    let h_int = TreeHash::hash(&int_val);
    let h_float = TreeHash::hash(&float_val);
    assert_ne!(h_int, h_float, "Integer vs Float must produce different hashes");
}

#[test]
fn hash_string_vs_null() {
    let null = Value::Null;
    let str_null = s("null");
    assert_ne!(TreeHash::hash(&null), TreeHash::hash(&str_null));
}

#[test]
fn equals_same_structure() {
    let a = obj(vec![("name", s("test")), ("val", Value::Integer(42))]);
    let b = obj(vec![("name", s("test")), ("val", Value::Integer(42))]);
    assert!(TreeHash::equals(&a, &b));
}

#[test]
fn equals_different_values() {
    let a = obj(vec![("name", s("test"))]);
    let b = obj(vec![("name", s("other"))]);
    assert!(!TreeHash::equals(&a, &b));
}

#[test]
fn diff_added_field() {
    let a = obj(vec![("x", Value::Integer(1))]);
    let b = obj(vec![("x", Value::Integer(1)), ("y", Value::Integer(2))]);
    let diffs = TreeHash::diff(&a, &b, "");
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "y");
    assert!(matches!(&diffs[0].kind, DiffKind::Added(_)));
}

#[test]
fn diff_removed_field() {
    let a = obj(vec![("x", Value::Integer(1)), ("y", Value::Integer(2))]);
    let b = obj(vec![("x", Value::Integer(1))]);
    let diffs = TreeHash::diff(&a, &b, "");
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "y");
    assert!(matches!(&diffs[0].kind, DiffKind::Removed(_)));
}

#[test]
fn diff_changed_field() {
    let a = obj(vec![("x", Value::Integer(1))]);
    let b = obj(vec![("x", Value::Integer(99))]);
    let diffs = TreeHash::diff(&a, &b, "");
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "x");
    assert!(matches!(&diffs[0].kind, DiffKind::Changed { .. }));
}

#[test]
fn diff_nested() {
    let a = obj(vec![("outer", obj(vec![("inner", Value::Integer(1))]))]);
    let b = obj(vec![("outer", obj(vec![("inner", Value::Integer(2))]))]);
    let diffs = TreeHash::diff(&a, &b, "");
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "outer.inner");
}

#[test]
fn hash_tree_detects_duplicates() {
    let schema = obj(vec![("type", s("string")), ("format", s("email"))]);
    let data = obj(vec![
        ("field1", schema.clone()),
        ("field2", schema.clone()),
        ("field3", schema.clone()),
    ]);

    let result = TreeHash::hash_tree(&data, "", 2, false);
    assert!(!result.root_hash.is_empty());

    // The repeated schema should appear with count >= 3
    let has_repeated = result.index.values().any(|e| e.count >= 3);
    assert!(has_repeated, "hash_tree should detect the 3x repeated schema");
}

#[test]
fn hash_tree_normalize() {
    // Same object, different key order — normalized should produce same hash
    let a = obj(vec![("b", Value::Integer(2)), ("a", Value::Integer(1))]);
    let b = obj(vec![("a", Value::Integer(1)), ("b", Value::Integer(2))]);

    let ha = TreeHash::hash_tree(&a, "", 1, true);
    let hb = TreeHash::hash_tree(&b, "", 1, true);
    assert_eq!(ha.root_hash, hb.root_hash, "Normalized hash should match regardless of key order");
}
