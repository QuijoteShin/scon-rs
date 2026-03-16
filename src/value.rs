// src/value.rs
// SCON Value type — preserves Object vs Array distinction
//
// Design decisions:
//   - CompactString instead of String: inline storage ≤24 bytes without heap allocation.
//     Most SCON keys ("name", "type", "id", "status") are <24 bytes → zero heap allocs.
//     Same stack size as String (24 bytes), transparent Deref<Target=str>.
//     This single change reduced decode overhead from 1.9x to 1.6x vs serde_json.
//
//   - IndexMap instead of HashMap: preserves insertion order for round-trip fidelity
//     (keys come out in the same order they went in). Backed by a Vec for ordered iteration.
//
//   - ahash instead of SipHash: ~2x faster hashing for short keys (most SCON keys are <24 bytes).
//     SipHash is DoS-resistant but SCON doesn't process untrusted hash keys at scale.
//     Measured ~10-24% improvement across encode/decode.
//
//   - Separate Integer(i64) vs Float(f64): SCON preserves numeric types from source data.
//     JSON spec has only "number", so serde_json uses Number which defers parsing.
//     SCON eagerly classifies: all-digits → i64, has-dot/exp → f64.

use compact_str::CompactString;
use indexmap::IndexMap;
use std::fmt;

// ahash: O(1) per hash but ~2x faster constant than SipHash for short keys
pub type SconMap<K, V> = IndexMap<K, V, ahash::RandomState>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    // CompactString: inline ≤24 bytes (sin heap alloc), heap solo para strings largas
    String(CompactString),
    Array(Vec<Value>),
    // CompactString keys: inline ≤24 bytes — la mayoría de keys SCON ("name", "type", "id") caben sin heap
    Object(SconMap<CompactString, Value>),
}

impl Value {
    pub fn is_primitive(&self) -> bool {
        matches!(self, Value::Null | Value::Bool(_) | Value::Integer(_) | Value::Float(_) | Value::String(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(n) => Some(*n),
            Value::Integer(n) => Some(*n as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&SconMap<CompactString, Value>> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{}", if *b { "true" } else { "false" }),
            Value::Integer(n) => write!(f, "{}", n),
            Value::Float(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    write!(f, "{:.1}", n)
                } else {
                    write!(f, "{}", n)
                }
            }
            Value::String(s) => write!(f, "{}", s),
            Value::Array(_) => write!(f, "[Array]"),
            Value::Object(_) => write!(f, "[Object]"),
        }
    }
}

// Conversion from serde_json::Value (for benchmarks)
impl From<&serde_json::Value> for Value {
    fn from(v: &serde_json::Value) -> Self {
        json_to_scon(v)
    }
}

pub fn json_to_scon(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::String(CompactString::from(s.as_str())),
        serde_json::Value::Array(arr) => {
            Value::Array(arr.iter().map(json_to_scon).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut map = SconMap::with_capacity_and_hasher(obj.len(), ahash::RandomState::new());
            for (k, v) in obj {
                map.insert(CompactString::from(k.as_str()), json_to_scon(v));
            }
            Value::Object(map)
        }
    }
}

pub fn scon_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Integer(n) => serde_json::json!(*n),
        Value::Float(n) => serde_json::json!(*n),
        Value::String(s) => serde_json::Value::String(s.to_string()),
        Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(scon_to_json).collect())
        }
        Value::Object(obj) => {
            let mut map = serde_json::Map::new();
            for (k, v) in obj {
                map.insert(k.to_string(), scon_to_json(v));
            }
            serde_json::Value::Object(map)
        }
    }
}
