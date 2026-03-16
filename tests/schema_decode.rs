// tests/schema_decode.rs
// Decode SCON with s: definitions and @s: references, verify resolved output.

use scon::*;

#[test]
fn decode_schema_def_and_ref() {
    let scon = "\
s:Email {type: string, format: email}
user:
 name: Alice
 email: @s:Email";

    let decoded = decode(scon).unwrap();
    if let Value::Object(root) = &decoded {
        if let Some(Value::Object(user)) = root.get("user") {
            if let Some(Value::Object(email)) = user.get("email") {
                assert_eq!(email.get("type").and_then(|v| v.as_str()), Some("string"));
                assert_eq!(email.get("format").and_then(|v| v.as_str()), Some("email"));
                return;
            }
        }
    }
    panic!("Schema reference not resolved");
}

#[test]
fn decode_multiple_refs() {
    let scon = "\
s:Id {type: integer}
s:Name {type: string}
item:
 id: @s:Id
 name: @s:Name";

    let decoded = decode(scon).unwrap();
    if let Value::Object(root) = &decoded {
        if let Some(Value::Object(item)) = root.get("item") {
            if let Value::Object(id) = item.get("id").unwrap() {
                assert_eq!(id.get("type").and_then(|v| v.as_str()), Some("integer"));
            } else {
                panic!("id ref not resolved to object");
            }
            if let Value::Object(name) = item.get("name").unwrap() {
                assert_eq!(name.get("type").and_then(|v| v.as_str()), Some("string"));
            } else {
                panic!("name ref not resolved to object");
            }
            return;
        }
    }
    panic!("Multiple refs not resolved");
}

#[test]
fn decode_ref_with_override() {
    let scon = "\
s:Base {type: object, required: true}
field: @s:Base {required: false}";

    let decoded = decode(scon).unwrap();
    if let Value::Object(root) = &decoded {
        if let Some(Value::Object(field)) = root.get("field") {
            assert_eq!(field.get("type").and_then(|v| v.as_str()), Some("object"));
            assert_eq!(field.get("required"), Some(&Value::Bool(false)));
            return;
        }
    }
    panic!("Override ref not resolved");
}

#[test]
fn decode_unresolvable_ref_returns_string() {
    let scon = "field: @s:NonExistent";
    let decoded = decode(scon).unwrap();
    if let Value::Object(root) = &decoded {
        // Unresolvable ref should be kept as string
        if let Some(Value::String(s)) = root.get("field") {
            assert!(s.contains("NonExistent"));
            return;
        }
    }
    panic!("Unresolvable ref should be kept as string");
}

#[test]
fn decode_response_ref() {
    let scon = "\
r:Success {status: 200, body: ok}
result: @r:Success";

    let decoded = decode(scon).unwrap();
    if let Value::Object(root) = &decoded {
        if let Some(Value::Object(result)) = root.get("result") {
            assert_eq!(result.get("status"), Some(&Value::Integer(200)));
            return;
        }
    }
    panic!("Response ref not resolved");
}

#[test]
fn tape_decode_with_schema_refs() {
    let scon = "\
s:Type {type: string}
item:
 field: @s:Type";

    let mut td = TapeDecoder::new();
    let tape = td.decode(scon).unwrap();

    // Tape should have: Object(1) → Key("item") → Object(1) → Key("field") → Object(1) → Key("type") → String("string")
    assert!(tape.nodes.len() >= 5, "Tape should have resolved the schema ref");

    // Find "type" key and "string" value in the tape
    let has_type_key = tape.nodes.iter().any(|n| matches!(n, Node::Key(k) if *k == "type"));
    let has_string_val = tape.nodes.iter().any(|n| matches!(n, Node::String(s) if *s == "string"));
    assert!(has_type_key, "Resolved schema should contain 'type' key");
    assert!(has_string_val, "Resolved schema should contain 'string' value");
}

#[test]
fn borrowed_decode_with_schema_refs() {
    let scon = "\
s:Type {type: string}
item:
 field: @s:Type";

    let arena = bumpalo::Bump::new();
    let mut bd = BorrowedDecoder::new(&arena);
    let decoded = bd.decode(scon).unwrap();

    if let BorrowedValue::Object(root) = &decoded {
        if let Some(BorrowedValue::Object(item)) = root.get("item") {
            if let Some(BorrowedValue::Object(field)) = item.get("field") {
                assert_eq!(
                    field.get("type"),
                    Some(&BorrowedValue::String("string"))
                );
                return;
            }
        }
    }
    panic!("Borrowed decoder should resolve schema refs");
}
