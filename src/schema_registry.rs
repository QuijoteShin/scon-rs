// src/schema_registry.rs
// Schema registry for SCON definition types (s:, r:, sec:).
//
// Stores named definitions and resolves references with cycle detection.
// Deep merge with dot-notation override paths matches the PHP implementation's
// resolveWithOverride behavior for schema composition.

use crate::value::{Value, SconMap};
use compact_str::CompactString;
use indexmap::IndexMap;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DefType {
    Schema,
    Response,
    Security,
}

impl DefType {
    pub fn from_prefix(s: &str) -> Option<Self> {
        match s {
            "s" => Some(DefType::Schema),
            "r" => Some(DefType::Response),
            "sec" => Some(DefType::Security),
            _ => None,
        }
    }

    pub fn prefix(&self) -> &'static str {
        match self {
            DefType::Schema => "s",
            DefType::Response => "r",
            DefType::Security => "sec",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SchemaRegistry {
    schemas: IndexMap<CompactString, Value>,
    responses: IndexMap<CompactString, Value>,
    security: IndexMap<CompactString, Value>,
    resolving: HashSet<String>,
}

impl Default for SchemaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self {
            schemas: IndexMap::new(),
            responses: IndexMap::new(),
            security: IndexMap::new(),
            resolving: HashSet::new(),
        }
    }

    pub fn register(&mut self, def_type: DefType, name: &str, definition: Value) {
        let key = CompactString::from(name);
        match def_type {
            DefType::Schema => { self.schemas.insert(key, definition); }
            DefType::Response => { self.responses.insert(key, definition); }
            DefType::Security => { self.security.insert(key, definition); }
        }
    }

    pub fn resolve(&mut self, def_type: DefType, name: &str) -> Result<Value, String> {
        let store = self.get_store(def_type);
        let val = store.get(name)
            .ok_or_else(|| format!("Undefined reference: @{}:{}", def_type.prefix(), name))?
            .clone();

        let ref_key = format!("{}:{}", def_type.prefix(), name);
        if self.resolving.contains(&ref_key) {
            // Circular reference — return $ref marker
            let mut marker = SconMap::default();
            marker.insert(
                CompactString::from("$ref"),
                Value::String(CompactString::from(format!("#/definitions/{}", name))),
            );
            return Ok(Value::Object(marker));
        }

        self.resolving.insert(ref_key.clone());
        let resolved = self.deep_resolve_refs(val)?;
        self.resolving.remove(&ref_key);

        Ok(resolved)
    }

    pub fn resolve_with_override(
        &mut self, def_type: DefType, name: &str, overrides: &Value
    ) -> Result<Value, String> {
        let mut base = self.resolve(def_type, name)?;

        if let Value::Object(ovr) = overrides {
            let mut removals = Vec::new();
            let mut merges = SconMap::default();

            for (key, val) in ovr {
                if key.starts_with('-') {
                    removals.push(key[1..].to_string());
                } else {
                    merges.insert(key.clone(), val.clone());
                }
            }

            // Apply removals
            for field in &removals {
                if field.contains('.') {
                    unset_dot_path(&mut base, field);
                } else if let Value::Object(ref mut obj) = base {
                    obj.shift_remove(field.as_str());
                }
            }

            // Apply deep merges with dot-notation
            for (key, val) in merges {
                let key_str = key.to_string();
                if key_str.contains('.') {
                    set_dot_path(&mut base, &key_str, val);
                } else if let Value::Object(ref mut obj) = base {
                    if let (Value::Object(ref existing), Value::Object(_)) = (obj.get(&key).cloned().unwrap_or(Value::Null), &val) {
                        if !is_sequential(&val) {
                            let merged = deep_merge(&Value::Object(existing.clone()), &val);
                            obj.insert(key, merged);
                            continue;
                        }
                    }
                    obj.insert(key, val);
                }
            }
        }

        Ok(base)
    }

    pub fn has(&self, def_type: DefType, name: &str) -> bool {
        self.get_store(def_type).contains_key(name)
    }

    pub fn get_all(&self, def_type: DefType) -> &IndexMap<CompactString, Value> {
        match def_type {
            DefType::Schema => &self.schemas,
            DefType::Response => &self.responses,
            DefType::Security => &self.security,
        }
    }

    pub fn reset(&mut self) {
        self.schemas.clear();
        self.responses.clear();
        self.security.clear();
        self.resolving.clear();
    }

    fn get_store(&self, def_type: DefType) -> &IndexMap<CompactString, Value> {
        match def_type {
            DefType::Schema => &self.schemas,
            DefType::Response => &self.responses,
            DefType::Security => &self.security,
        }
    }

    // Deep-resolve any @ref markers within a definition
    fn deep_resolve_refs(&mut self, data: Value) -> Result<Value, String> {
        match data {
            Value::Object(obj) => {
                let mut result = SconMap::with_capacity_and_hasher(obj.len(), ahash::RandomState::new());
                for (key, val) in obj {
                    match &val {
                        Value::Object(inner) => {
                            if let Some(Value::Object(ref_obj)) = inner.get("@ref") {
                                // Has @ref — resolve it
                                let ref_type = ref_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                let ref_name = ref_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                if let Some(dt) = DefType::from_prefix(ref_type) {
                                    if let Some(Value::Object(overrides)) = inner.get("@overrides") {
                                        let ovr = Value::Object(overrides.clone());
                                        result.insert(key, self.resolve_with_override(dt, ref_name, &ovr)?);
                                    } else {
                                        result.insert(key, self.resolve(dt, ref_name)?);
                                    }
                                } else {
                                    result.insert(key, val);
                                }
                            } else if inner.contains_key("@polymorphic") {
                                if let Some(Value::Array(refs)) = inner.get("@polymorphic") {
                                    let resolved = self.resolve_polymorphic(refs)?;
                                    result.insert(key, resolved);
                                } else {
                                    result.insert(key, val);
                                }
                            } else {
                                result.insert(key, self.deep_resolve_refs(val)?);
                            }
                        }
                        Value::Array(arr) => {
                            let resolved: Result<Vec<Value>, String> = arr.iter()
                                .map(|item| self.deep_resolve_refs(item.clone()))
                                .collect();
                            result.insert(key, Value::Array(resolved?));
                        }
                        _ => { result.insert(key, val); }
                    }
                }
                Ok(Value::Object(result))
            }
            Value::Array(arr) => {
                let resolved: Result<Vec<Value>, String> = arr.into_iter()
                    .map(|item| self.deep_resolve_refs(item))
                    .collect();
                Ok(Value::Array(resolved?))
            }
            other => Ok(other),
        }
    }

    fn resolve_polymorphic(&mut self, refs: &[Value]) -> Result<Value, String> {
        let mut schemas = Vec::new();
        for r in refs {
            if let Value::Object(ref_obj) = r {
                let ref_type = ref_obj.get("type").and_then(|v| v.as_str()).unwrap_or("s");
                let ref_name = ref_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(dt) = DefType::from_prefix(ref_type) {
                    schemas.push(self.resolve(dt, ref_name)?);
                }
            }
        }
        let mut result = SconMap::default();
        result.insert(CompactString::from("oneOf"), Value::Array(schemas));
        Ok(Value::Object(result))
    }
}

// --- Helper functions ---

fn deep_merge(base: &Value, override_val: &Value) -> Value {
    match (base, override_val) {
        (Value::Object(base_obj), Value::Object(ovr_obj)) => {
            let mut result = base_obj.clone();
            for (key, val) in ovr_obj {
                if let Some(existing) = result.get(key) {
                    if let (Value::Object(_), Value::Object(_)) = (existing, val) {
                        if !is_sequential(val) && !is_sequential(existing) {
                            result.insert(key.clone(), deep_merge(existing, val));
                            continue;
                        }
                    }
                }
                result.insert(key.clone(), val.clone());
            }
            Value::Object(result)
        }
        _ => override_val.clone(),
    }
}

fn set_dot_path(obj: &mut Value, path: &str, val: Value) {
    let keys: Vec<&str> = path.split('.').collect();
    let mut current = obj;
    for (i, key) in keys.iter().enumerate() {
        if i == keys.len() - 1 {
            if let Value::Object(ref mut map) = current {
                map.insert(CompactString::from(*key), val);
                return;
            }
        } else {
            if let Value::Object(ref mut map) = current {
                if !map.contains_key(*key) || !matches!(map.get(*key), Some(Value::Object(_))) {
                    map.insert(CompactString::from(*key), Value::Object(SconMap::default()));
                }
                current = map.get_mut(*key).unwrap();
            } else {
                return;
            }
        }
    }
}

fn unset_dot_path(obj: &mut Value, path: &str) {
    let keys: Vec<&str> = path.split('.').collect();
    let mut current = obj;
    for (i, key) in keys.iter().enumerate() {
        if i == keys.len() - 1 {
            if let Value::Object(ref mut map) = current {
                map.shift_remove(*key);
            }
        } else {
            if let Value::Object(ref mut map) = current {
                if let Some(child) = map.get_mut(*key) {
                    current = child;
                } else {
                    return;
                }
            } else {
                return;
            }
        }
    }
}

fn is_sequential(val: &Value) -> bool {
    matches!(val, Value::Array(_))
}
