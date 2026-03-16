# scon

**SCON — Schema-Compact Object Notation**

A human-readable data serialization format with structural deduplication. 59-66% payload reduction, 64% fewer LLM tokens, no binary encoding.

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

Single-pass tape decoder, 500 iterations, release mode, tracking allocator:

| Dataset | Decode | Peak memory | Payload reduction |
|---------|-------:|----------:|----------------:|
| OpenAPI (71 endpoints) | 0.195 ms | 3,874 KB | **-66% (dedup)** |
| DB (24 DDL schemas) | 0.045 ms | 3,512 KB | **-29%** |
| Config (40 records) | 0.292 ms | 4,111 KB | -8% |
| ISA-95 (equipment hierarchy) | 0.011 ms | 4,568 KB | **-87% (dedup)** |

LLM token efficiency (cl100k_base): **64% fewer tokens** on OpenAPI specs — less context window waste for RAG pipelines and tool-use agents.

Full methodology: [DOI 10.5281/zenodo.14733092](https://doi.org/10.5281/zenodo.14733092)
Benchmarks, optimization log (21 phases), and industrial protocol fixtures: [github.com/QuijoteShin/scon](https://github.com/QuijoteShin/scon)

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
