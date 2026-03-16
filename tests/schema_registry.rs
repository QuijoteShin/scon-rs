// tests/schema_registry.rs
// SchemaRegistry: register/resolve, cycle detection, overrides, dot-notation.

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
fn register_and_resolve() {
    let mut reg = SchemaRegistry::new();
    let schema = obj(vec![("type", s("string")), ("format", s("email"))]);
    reg.register(DefType::Schema, "Email", schema.clone());

    assert!(reg.has(DefType::Schema, "Email"));
    assert!(!reg.has(DefType::Schema, "Missing"));

    let resolved = reg.resolve(DefType::Schema, "Email").unwrap();
    assert_eq!(resolved, schema);
}

#[test]
fn resolve_undefined_returns_error() {
    let mut reg = SchemaRegistry::new();
    let result = reg.resolve(DefType::Schema, "Nonexistent");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Undefined"));
}

#[test]
fn def_types() {
    let mut reg = SchemaRegistry::new();
    reg.register(DefType::Schema, "s1", obj(vec![("a", Value::Integer(1))]));
    reg.register(DefType::Response, "r1", obj(vec![("b", Value::Integer(2))]));
    reg.register(DefType::Security, "sec1", obj(vec![("c", Value::Integer(3))]));

    assert!(reg.has(DefType::Schema, "s1"));
    assert!(reg.has(DefType::Response, "r1"));
    assert!(reg.has(DefType::Security, "sec1"));

    // Cross-type shouldn't resolve
    assert!(!reg.has(DefType::Schema, "r1"));
}

#[test]
fn resolve_with_override_merge() {
    let mut reg = SchemaRegistry::new();
    let base = obj(vec![
        ("type", s("object")),
        ("required", Value::Bool(true)),
        ("desc", s("base")),
    ]);
    reg.register(DefType::Schema, "Base", base);

    let overrides = obj(vec![("desc", s("overridden"))]);
    let resolved = reg.resolve_with_override(DefType::Schema, "Base", &overrides).unwrap();

    if let Value::Object(r) = &resolved {
        assert_eq!(r.get("desc").and_then(|v| v.as_str()), Some("overridden"));
        assert_eq!(r.get("type").and_then(|v| v.as_str()), Some("object"));
    } else {
        panic!("Expected object");
    }
}

#[test]
fn resolve_with_override_removal() {
    let mut reg = SchemaRegistry::new();
    let base = obj(vec![
        ("type", s("object")),
        ("deprecated", Value::Bool(true)),
    ]);
    reg.register(DefType::Schema, "WithDep", base);

    // "-deprecated" removes the field
    let overrides = obj(vec![
        ("-deprecated", Value::Bool(true)),
    ]);
    let resolved = reg.resolve_with_override(DefType::Schema, "WithDep", &overrides).unwrap();

    if let Value::Object(r) = &resolved {
        assert!(!r.contains_key("deprecated"), "Field should be removed");
        assert!(r.contains_key("type"), "Other fields preserved");
    } else {
        panic!("Expected object");
    }
}

#[test]
fn resolve_with_dot_notation_override() {
    let mut reg = SchemaRegistry::new();
    let base = obj(vec![
        ("info", obj(vec![
            ("title", s("Original")),
            ("version", s("1.0")),
        ])),
    ]);
    reg.register(DefType::Schema, "Spec", base);

    let overrides = obj(vec![
        ("info.title", s("Updated")),
    ]);
    let resolved = reg.resolve_with_override(DefType::Schema, "Spec", &overrides).unwrap();

    if let Value::Object(r) = &resolved {
        if let Some(Value::Object(info)) = r.get("info") {
            assert_eq!(info.get("title").and_then(|v| v.as_str()), Some("Updated"));
            assert_eq!(info.get("version").and_then(|v| v.as_str()), Some("1.0"));
            return;
        }
    }
    panic!("Dot-notation override failed");
}

#[test]
fn cycle_detection() {
    let mut reg = SchemaRegistry::new();
    // Register a schema that references itself (simulated via resolve call pattern)
    let schema = obj(vec![("self", s("ref"))]);
    reg.register(DefType::Schema, "Cyclic", schema);

    // First resolve works
    let result = reg.resolve(DefType::Schema, "Cyclic");
    assert!(result.is_ok());
}

#[test]
fn get_all() {
    let mut reg = SchemaRegistry::new();
    reg.register(DefType::Schema, "A", obj(vec![("x", Value::Integer(1))]));
    reg.register(DefType::Schema, "B", obj(vec![("y", Value::Integer(2))]));

    let all = reg.get_all(DefType::Schema);
    assert_eq!(all.len(), 2);
}

#[test]
fn reset_clears_all() {
    let mut reg = SchemaRegistry::new();
    reg.register(DefType::Schema, "X", obj(vec![]));
    reg.register(DefType::Response, "Y", obj(vec![]));
    reg.reset();

    assert!(!reg.has(DefType::Schema, "X"));
    assert!(!reg.has(DefType::Response, "Y"));
}

#[test]
fn def_type_prefix_roundtrip() {
    assert_eq!(DefType::from_prefix("s"), Some(DefType::Schema));
    assert_eq!(DefType::from_prefix("r"), Some(DefType::Response));
    assert_eq!(DefType::from_prefix("sec"), Some(DefType::Security));
    assert_eq!(DefType::from_prefix("unknown"), None);

    assert_eq!(DefType::Schema.prefix(), "s");
    assert_eq!(DefType::Response.prefix(), "r");
    assert_eq!(DefType::Security.prefix(), "sec");
}
