// tests/tabular_schema_ref.rs
// Tabular arrays with @s: schema references for field names

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

fn s(v: &str) -> Value {
    Value::String(CompactString::from(v))
}

#[test]
fn tabular_with_schema_ref_owned() {
    let input = "\
s:Sensor {sensorType: string, value: number, unit: string}
readings[2]@s:Sensor:
 temperature, 23.5, °C
 humidity, 45.2, %";

    let decoded = decode(input).unwrap();
    if let Value::Object(root) = &decoded {
        if let Some(Value::Array(readings)) = root.get("readings") {
            assert_eq!(readings.len(), 2);
            if let Value::Object(row0) = &readings[0] {
                assert_eq!(row0.get("sensorType").and_then(|v| v.as_str()), Some("temperature"));
                assert_eq!(row0.get("value").and_then(|v| v.as_f64()), Some(23.5));
                assert_eq!(row0.get("unit").and_then(|v| v.as_str()), Some("°C"));
                return;
            }
        }
    }
    panic!("Tabular schema ref decode failed");
}

#[test]
fn tabular_schema_ref_encode_roundtrip() {
    let schema = obj(vec![
        ("sensorType", s("string")),
        ("value", s("number")),
        ("unit", s("string")),
    ]);
    let data = obj(vec![(
        "readings",
        Value::Array(vec![
            obj(vec![
                ("sensorType", s("temperature")),
                ("value", Value::Float(23.5)),
                ("unit", s("C")),
            ]),
            obj(vec![
                ("sensorType", s("humidity")),
                ("value", Value::Float(45.2)),
                ("unit", s("%")),
            ]),
        ]),
    )]);

    let encoded = Encoder::new()
        .with_schemas(vec![("Sensor", schema.clone())])
        .encode(&data);

    assert!(encoded.contains("readings[2]@s:Sensor:"));
    assert!(!encoded.contains("readings[2]{sensorType,value,unit}:"));

    let decoded = decode(&encoded).unwrap();
    assert_eq!(decoded, data);

    let re_encoded = Encoder::new()
        .with_schemas(vec![("Sensor", schema)])
        .encode(&decoded);
    assert_eq!(re_encoded, encoded);
}

#[test]
fn tabular_schema_ref_encode_falls_back_when_layout_mismatches() {
    let schema = obj(vec![
        ("value", s("number")),
        ("sensorType", s("string")),
        ("unit", s("string")),
    ]);
    let data = obj(vec![(
        "readings",
        Value::Array(vec![
            obj(vec![
                ("sensorType", s("temperature")),
                ("value", Value::Float(23.5)),
                ("unit", s("C")),
            ]),
            obj(vec![
                ("sensorType", s("humidity")),
                ("value", Value::Float(45.2)),
                ("unit", s("%")),
            ]),
        ]),
    )]);

    let encoded = Encoder::new()
        .with_schemas(vec![("Sensor", schema)])
        .encode(&data);

    assert!(encoded.contains("readings[2]{sensorType,value,unit}:"));
    assert!(!encoded.contains("readings[2]@s:Sensor:"));
}

#[test]
fn tabular_schema_ref_reuse() {
    let input = "\
s:Sensor {sensorType: string, value: number, unit: string}
readings[2]@s:Sensor:
 temperature, 23.5, °C
 humidity, 45.2, %
alerts[1]@s:Sensor:
 pressure, 105.2, bar";

    let decoded = decode(input).unwrap();
    if let Value::Object(root) = &decoded {
        let readings = root.get("readings").unwrap();
        let alerts = root.get("alerts").unwrap();
        if let (Value::Array(r), Value::Array(a)) = (readings, alerts) {
            assert_eq!(r.len(), 2);
            assert_eq!(a.len(), 1);
            if let Value::Object(alert) = &a[0] {
                assert_eq!(alert.get("sensorType").and_then(|v| v.as_str()), Some("pressure"));
                return;
            }
        }
    }
    panic!("Schema ref reuse failed");
}

#[test]
fn tabular_schema_ref_tape() {
    let input = "\
s:Sensor {sensorType: string, value: number}
readings[2]@s:Sensor:
 temperature, 23.5
 humidity, 45.2";

    let mut td = TapeDecoder::new();
    let tape = td.decode(input).unwrap();

    // Should have: Object → Key("readings") → Array(2) → Object(2) → Key/Val pairs...
    let has_sensor_key = tape.nodes.iter().any(|n| matches!(n, Node::Key(k) if *k == "sensorType"));
    let has_temp = tape.nodes.iter().any(|n| matches!(n, Node::String(s) if *s == "temperature"));
    assert!(has_sensor_key, "Tape should resolve schema ref fields");
    assert!(has_temp, "Tape should contain row data");
}

#[test]
fn tabular_schema_ref_borrowed() {
    let input = "\
s:Sensor {sensorType: string, value: number}
readings[2]@s:Sensor:
 temperature, 23.5
 humidity, 45.2";

    let arena = bumpalo::Bump::new();
    let mut bd = BorrowedDecoder::new(&arena);
    let decoded = bd.decode(input).unwrap();

    if let BorrowedValue::Object(root) = &decoded {
        if let Some(BorrowedValue::Array(readings)) = root.get("readings") {
            assert_eq!(readings.len(), 2);
            if let BorrowedValue::Object(row0) = &readings[0] {
                assert_eq!(row0.get("sensorType"), Some(&BorrowedValue::String("temperature")));
                return;
            }
        }
    }
    panic!("Borrowed tabular schema ref failed");
}

#[test]
fn tabular_schema_ref_undefined_errors() {
    let input = "\
readings[2]@s:NonExistent:
 a, b
 c, d";

    let result = decode(input);
    assert!(result.is_err(), "Undefined schema ref should error");
}
