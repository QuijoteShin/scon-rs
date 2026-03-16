# scon

**SCON — Schema-Compact Object Notation**

A human-readable data serialization format with structural deduplication. Smaller than JSON, faster to parse, no binary encoding.

[![Crates.io](https://img.shields.io/crates/v/scon.svg)](https://crates.io/crates/scon)
[![docs.rs](https://docs.rs/scon/badge.svg)](https://docs.rs/scon)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## Quick Start

```rust
use scon::{encode, decode, Value};

let mut data = scon::value::SconMap::new();
data.insert("name".into(), Value::String("scon".into()));
data.insert("version".into(), Value::Integer(1));
let obj = Value::Object(data);

let scon_str = encode(&obj);
let parsed = decode(&scon_str).unwrap();
assert_eq!(parsed, obj);
```

## Features

- **3 decoder modes**: Owned (two-pass), Borrowed (zero-copy arena), Tape (single-pass flat)
- **Structural dedup**: xxHash128 fingerprinting detects repeated subtrees, replaces with schema references — 59-66% payload reduction on OpenAPI specs
- **Schema subsystem**: Registry with cycle detection, deep merge, dot-notation overrides
- **Validation**: Loose/Warn/Strict modes with OpenAPI enforcement rules
- **Minifier**: Bidirectional minify (indent to semicolons) and expand

## Performance

The tape decoder beats simd-json on 2/3 benchmark datasets:

| Dataset | vs simd-json | vs serde_json |
|---------|-------------|---------------|
| OpenAPI | **21% faster** | 53% faster |
| DB | **18% faster** | 35% faster |
| Config | 8% slower | 40% faster |

500 iterations, release mode, tracking allocator. Full methodology: [DOI 10.5281/zenodo.14733092](https://doi.org/10.5281/zenodo.14733092)

## Format

```
name: scon
version: 1
features:
  dedup: true
  modes[3]: owned, borrowed, tape
endpoints[2]{method,path,status}:
  GET, /api/users, 200
  POST, /api/users, 201
```

No braces, no brackets, no repeated keys in uniform arrays, unquoted strings, built-in schema dedup via `@s:name` references.

## API

```rust
use scon::*;

// Encode / Decode
let encoded = encode(&value);
let decoded = decode(&scon_string)?;

// Structural dedup (auto-extracts repeated subtrees)
let deduped = encode_with_dedup(&value);

// Minify / Expand
let mini = minify(&scon_string);
let expanded = expand(&mini, 2);

// Tape decoder (single-pass, fastest)
let tape = TapeDecoder::new().decode(input)?;

// Borrowed decoder (zero-copy, bumpalo arena)
let arena = bumpalo::Bump::new();
let borrowed = BorrowedDecoder::new(&arena).decode(input)?;

// Validation
let result = Validator::new(ValidationMode::Strict)
    .validate(&value, &schema);
```

## License

MIT
