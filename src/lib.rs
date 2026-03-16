// src/lib.rs
// S.C.O.N. — Schema-Compact Object Notation
//
// SCON is a human-readable data serialization format that achieves smaller payloads
// than JSON by eliminating syntactic redundancy:
//   - No repeated keys in arrays of uniform objects (tabular encoding)
//   - No braces/brackets for nested structures (indentation-based)
//   - Unquoted strings when unambiguous (no special chars)
//   - Optional structural deduplication via TreeHash (@schema references)
//
// Format overview:
//   key: value                          # primitive key-value
//   parent:                             # nested object (children indented)
//     child: value
//   items[3]: a, b, c                   # inline array of primitives
//   rows[N]{col1,col2,...}:             # tabular array — header once, N data rows
//     val1, val2, ...                   #   (eliminates N×K key repetitions vs JSON)
//   list[N]:                            # expanded array — each item prefixed with "- "
//     - item1
//     - key: value                      #   (objects as list items)
//
// Minified form: semicolons replace newlines+indent
//   key: value;child: nested;;sibling   # ';' = newline, ';;' = dedent 1, ';;;' = dedent 2
//
// Architecture (this crate):
//   value.rs   — Value enum (Null|Bool|Integer|Float|String|Array|Object) with CompactString
//   encoder.rs — Value → SCON string, O(N) single-pass DFS with tabular detection
//   decoder.rs — SCON string → Value, O(L) two-pass (line classify + semantic parse)
//   minifier.rs — minify (indent→semicolons) and expand (semicolons→indent), both O(L)
//
// Performance vs serde_json (native Rust, compiled):
//   Encode: 1.1–2.0x (near parity on large datasets)
//   Decode: 1.6–1.8x (architectural gap: two-pass vs single-pass recursive descent)
//   Size:   13–29% smaller minified, up to 66% with dedup

pub mod value;
pub mod encoder;
pub mod decoder;
pub mod minifier;
pub mod borrowed;
pub mod tape;
pub mod treehash;
pub mod schema_registry;
pub mod validator;

pub use value::Value;
pub use encoder::Encoder;
pub use decoder::Decoder;
pub use minifier::Minifier;
pub use borrowed::{BorrowedDecoder, BorrowedValue};
pub use tape::{TapeDecoder, Tape, Node};
pub use treehash::{TreeHash, HashTreeResult, HashEntry, DiffEntry, DiffKind};
pub use schema_registry::{SchemaRegistry, DefType};
pub use validator::{Validator, ValidationMode, ValidationResult};

// Convenience functions — stateless wrappers, allocate encoder/decoder per call

pub fn encode(data: &Value) -> String {
    Encoder::new().encode(data)
}

pub fn encode_to(data: &Value, buf: &mut String) {
    Encoder::new().encode_to(data, buf);
}

pub fn encode_with_indent(data: &Value, indent: usize) -> String {
    Encoder::new().with_indent(indent).encode(data)
}

pub fn encode_with_dedup(data: &Value) -> String {
    Encoder::new().with_auto_extract(true).encode(data)
}

pub fn decode(input: &str) -> Result<Value, String> {
    Decoder::new().decode(input)
}

pub fn minify(scon: &str) -> String {
    Minifier::minify(scon)
}

pub fn expand(minified: &str, indent: usize) -> String {
    Minifier::expand(minified, indent)
}
