// tests/validator.rs
// Validator: modes (loose/warn/strict), required fields, enforce rules.

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
fn loose_mode_ignores_issues() {
    let data = s("not an object");
    let result = Validator::new(ValidationMode::Loose).validate(&data);
    assert!(result.valid);
    assert!(result.warnings.is_empty());
    assert!(result.errors.is_empty());
}

#[test]
fn warn_mode_collects_warnings() {
    let data = s("not an object");
    let result = Validator::new(ValidationMode::Warn).validate(&data);
    assert!(result.valid);
    assert!(!result.warnings.is_empty());
}

#[test]
fn strict_mode_collects_errors() {
    let data = s("not an object");
    let result = Validator::new(ValidationMode::Strict).validate(&data);
    assert!(!result.valid);
    assert!(!result.errors.is_empty());
}

#[test]
fn validate_against_schema_required_field() {
    let schema = obj(vec![
        ("name!", s("string")),
        ("age", s("integer")),
    ]);
    let data_ok = obj(vec![
        ("name", s("Alice")),
        ("age", Value::Integer(30)),
    ]);
    let data_missing = obj(vec![
        ("age", Value::Integer(30)),
    ]);

    let validator = Validator::new(ValidationMode::Strict);
    let result_ok = validator.validate_against_schema(&data_ok, &schema, "");
    assert!(result_ok.valid, "All required fields present");

    let result_fail = validator.validate_against_schema(&data_missing, &schema, "");
    assert!(!result_fail.valid, "Missing required field should fail strict");
    assert!(result_fail.errors.iter().any(|e| e.contains("name")));
}

#[test]
fn strict_mode_rejects_extra_fields() {
    let schema = obj(vec![("name", s("string"))]);
    let data = obj(vec![
        ("name", s("Alice")),
        ("extra", Value::Integer(1)),
    ]);

    let result = Validator::new(ValidationMode::Strict)
        .validate_against_schema(&data, &schema, "");
    assert!(!result.valid);
    assert!(result.errors.iter().any(|e| e.contains("extra")));
}

#[test]
fn warn_mode_warns_extra_fields() {
    let schema = obj(vec![("name", s("string"))]);
    let data = obj(vec![
        ("name", s("Alice")),
        ("extra", Value::Integer(1)),
    ]);

    let result = Validator::new(ValidationMode::Warn)
        .validate_against_schema(&data, &schema, "");
    assert!(result.valid);
    assert!(result.warnings.iter().any(|w| w.contains("extra")));
}

#[test]
fn loose_mode_no_extra_field_report() {
    let schema = obj(vec![("name", s("string"))]);
    let data = obj(vec![
        ("name", s("Alice")),
        ("extra", Value::Integer(1)),
    ]);

    let result = Validator::new(ValidationMode::Loose)
        .validate_against_schema(&data, &schema, "");
    assert!(result.valid);
    assert!(result.warnings.is_empty());
    assert!(result.errors.is_empty());
}

#[test]
fn enforce_openapi_required_fields() {
    let valid_spec = obj(vec![
        ("openapi", s("3.1.0")),
        ("info", obj(vec![
            ("title", s("My API")),
            ("version", s("1.0")),
        ])),
        ("paths", obj(vec![])),
    ]);

    let result = Validator::new(ValidationMode::Strict)
        .with_enforce("openapi:3.1")
        .validate(&valid_spec);
    assert!(result.valid, "Valid OpenAPI spec should pass: {:?}", result.errors);
}

#[test]
fn enforce_openapi_missing_fields() {
    let invalid_spec = obj(vec![
        ("openapi", s("3.1.0")),
        // Missing: info, paths
    ]);

    let result = Validator::new(ValidationMode::Strict)
        .with_enforce("openapi:3.1")
        .validate(&invalid_spec);
    assert!(!result.valid);
    assert!(result.errors.iter().any(|e| e.contains("info")));
    assert!(result.errors.iter().any(|e| e.contains("paths")));
}

#[test]
fn enforce_openapi_nested_required() {
    let spec = obj(vec![
        ("openapi", s("3.1.0")),
        ("info", obj(vec![
            // Missing: title, version
        ])),
        ("paths", obj(vec![])),
    ]);

    let result = Validator::new(ValidationMode::Strict)
        .with_enforce("openapi:3.1")
        .validate(&spec);
    assert!(!result.valid);
    assert!(result.errors.iter().any(|e| e.contains("info.title")));
    assert!(result.errors.iter().any(|e| e.contains("info.version")));
}

#[test]
fn validate_schema_empty() {
    let result = Validator::new(ValidationMode::Strict)
        .validate_schema("EmptySchema", &obj(vec![]));
    assert!(!result.valid);
    assert!(result.errors.iter().any(|e| e.contains("empty")));
}

#[test]
fn validation_result_metadata() {
    let result = Validator::new(ValidationMode::Warn)
        .with_enforce("openapi:3.1")
        .validate(&obj(vec![]));
    assert_eq!(result.mode, ValidationMode::Warn);
    assert_eq!(result.enforce, Some("openapi:3.1".to_string()));
}
