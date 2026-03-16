// src/treehash.rs
// Structural hashing for SCON Value trees.
//
// Two distinct algorithms serve different purposes:
//   fingerprint(): type-tagged binary → xxh3_128 (for equals/diff — type-safe, int !== float)
//   hash_tree(): serde_json canonical serialization → xxh3_128 (for dedup — JSON-semantic)
//
// The split is intentional: fingerprint preserves numeric type distinction (Integer vs Float)
// while hash_tree uses JSON serialization for dedup compatibility with PHP/JS implementations.
// PHP uses json_encode (C-level) for speed; we use serde_json::to_string (same semantics).

use crate::value::{Value, scon_to_json};
use compact_str::CompactString;
use indexmap::IndexMap;
use xxhash_rust::xxh3::xxh3_128;

pub struct TreeHash;

#[derive(Debug, Clone)]
pub struct HashTreeResult {
    pub root_hash: String,
    pub index: IndexMap<String, HashEntry>,
}

#[derive(Debug, Clone)]
pub struct HashEntry {
    pub count: usize,
    pub path: String,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffEntry {
    pub path: String,
    pub kind: DiffKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiffKind {
    Added(Value),
    Removed(Value),
    Changed { old: Value, new: Value },
}

impl TreeHash {
    pub fn hash(data: &Value) -> String {
        let fp = fingerprint(data);
        // Short fingerprints (primitives) are already hex-safe but not hashed;
        // compound types return 32-char hex from xxh3_128
        if fp.len() == 32 && fp.bytes().all(|b| b.is_ascii_hexdigit()) {
            fp
        } else {
            format!("{:032x}", xxh3_128(fp.as_bytes()))
        }
    }

    pub fn hash_tree(data: &Value, base_path: &str, min_keys: usize, normalize: bool) -> HashTreeResult {
        let mut data = data.clone();
        if normalize {
            recursive_ksort(&mut data);
        }

        let mut index = IndexMap::new();
        collect_hashes(&data, base_path, &mut index, min_keys);

        let json_val = scon_to_json(&data);
        let json_str = serde_json::to_string(&json_val).unwrap_or_default();
        let root_hash = format!("{:032x}", xxh3_128(json_str.as_bytes()));

        HashTreeResult { root_hash, index }
    }

    pub fn equals(a: &Value, b: &Value) -> bool {
        if a == b { return true; }
        fingerprint(a) == fingerprint(b)
    }

    pub fn diff(a: &Value, b: &Value, path: &str) -> Vec<DiffEntry> {
        if a == b { return vec![]; }
        if fingerprint(a) == fingerprint(b) { return vec![]; }

        let (obj_a, obj_b) = match (a, b) {
            (Value::Object(oa), Value::Object(ob)) => (oa, ob),
            _ => {
                return vec![DiffEntry {
                    path: path.to_string(),
                    kind: DiffKind::Changed { old: a.clone(), new: b.clone() },
                }];
            }
        };

        let mut diffs = Vec::new();

        // Collect all keys from both objects
        let mut all_keys: Vec<&CompactString> = Vec::new();
        for k in obj_a.keys() {
            all_keys.push(k);
        }
        for k in obj_b.keys() {
            if !obj_a.contains_key(k) {
                all_keys.push(k);
            }
        }

        for key in all_keys {
            let current_path = if path.is_empty() {
                key.to_string()
            } else {
                format!("{}.{}", path, key)
            };

            match (obj_a.get(key), obj_b.get(key)) {
                (None, Some(v)) => {
                    diffs.push(DiffEntry { path: current_path, kind: DiffKind::Added(v.clone()) });
                }
                (Some(v), None) => {
                    diffs.push(DiffEntry { path: current_path, kind: DiffKind::Removed(v.clone()) });
                }
                (Some(va), Some(vb)) => {
                    if va == vb { continue; }
                    if fingerprint(va) == fingerprint(vb) { continue; }
                    match (va, vb) {
                        (Value::Object(_), Value::Object(_)) => {
                            diffs.extend(Self::diff(va, vb, &current_path));
                        }
                        _ => {
                            diffs.push(DiffEntry {
                                path: current_path,
                                kind: DiffKind::Changed { old: va.clone(), new: vb.clone() },
                            });
                        }
                    }
                }
                (None, None) => unreachable!(),
            }
        }

        diffs
    }
}

// --- fingerprint internals ---
// Primitives return type-tagged binary strings (no hash call).
// Compounds return 32-char hex xxh3_128 digests.

fn fingerprint(data: &Value) -> String {
    match data {
        Value::Null => "\x00".to_string(),
        Value::Bool(true) => "\x01\x01".to_string(),
        Value::Bool(false) => "\x01\x00".to_string(),
        Value::Integer(n) => format!("\x02{}", n),
        Value::Float(n) => format!("\x03{}", n),
        Value::String(s) => format!("\x04{}", s),
        Value::Array(arr) => {
            if arr.is_empty() {
                return format!("{:032x}", xxh3_128(b"A:0"));
            }
            let mut buf = format!("A:{}", arr.len());
            for item in arr {
                buf.push('|');
                buf.push_str(&fingerprint(item));
            }
            format!("{:032x}", xxh3_128(buf.as_bytes()))
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                return format!("{:032x}", xxh3_128(b"O:0"));
            }
            // Sort keys for deterministic fingerprint (matches PHP's ksort in mapFP)
            let mut keys: Vec<&CompactString> = obj.keys().collect();
            keys.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            let mut buf = format!("O:{}", obj.len());
            for key in keys {
                buf.push('|');
                buf.push_str(key);
                buf.push(':');
                buf.push_str(&fingerprint(&obj[key]));
            }
            format!("{:032x}", xxh3_128(buf.as_bytes()))
        }
    }
}

// --- hash_tree internals ---

fn recursive_ksort(data: &mut Value) {
    match data {
        Value::Object(obj) => {
            obj.sort_by(|a, _, b, _| a.cmp(b));
            for (_, v) in obj.iter_mut() {
                recursive_ksort(v);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                recursive_ksort(item);
            }
        }
        _ => {}
    }
}

fn collect_hashes(data: &Value, path: &str, index: &mut IndexMap<String, HashEntry>, min_keys: usize) {
    match data {
        Value::Object(obj) => {
            for (key, val) in obj {
                let child_path = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{}.{}", path, key)
                };

                match val {
                    Value::Array(arr) => {
                        // Traverse lists to find sub-objects (oneOf, allOf, items)
                        for (i, item) in arr.iter().enumerate() {
                            if let Value::Object(inner) = item {
                                if !inner.is_empty() {
                                    let item_path = format!("{}.[{}]", child_path, i);
                                    if inner.len() >= min_keys {
                                        hash_and_register(item, &item_path, index);
                                    }
                                    collect_hashes(item, &item_path, index, min_keys);
                                }
                            }
                        }
                    }
                    Value::Object(inner) => {
                        if !inner.is_empty() {
                            if inner.len() >= min_keys {
                                hash_and_register(val, &child_path, index);
                            }
                            collect_hashes(val, &child_path, index, min_keys);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let child_path = if path.is_empty() {
                    format!("[{}]", i)
                } else {
                    format!("{}.[{}]", path, i)
                };
                if let Value::Object(inner) = item {
                    if !inner.is_empty() {
                        if inner.len() >= min_keys {
                            hash_and_register(item, &child_path, index);
                        }
                        collect_hashes(item, &child_path, index, min_keys);
                    }
                }
            }
        }
        _ => {}
    }
}

fn hash_and_register(val: &Value, path: &str, index: &mut IndexMap<String, HashEntry>) {
    let json_val = scon_to_json(val);
    if let Ok(json_str) = serde_json::to_string(&json_val) {
        let hash = format!("{:032x}", xxh3_128(json_str.as_bytes()));
        if let Some(entry) = index.get_mut(&hash) {
            entry.count += 1;
        } else {
            index.insert(hash, HashEntry {
                count: 1,
                path: path.to_string(),
                data: val.clone(),
            });
        }
    }
}
