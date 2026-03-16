// src/encoder.rs
// SCON Encoder — Value → SCON string
//
// Architecture: single-pass recursive DFS over Value tree → String buffer.
// Time O(N) where N = total nodes. Space O(D + L) where D = max depth, L = output length.
//
// Key encoding strategies:
//   1. Primitives: written inline after "key: " — unquoted when safe, quoted with escapes otherwise
//   2. Nested objects: "key:\n" then children indented by `self.indent` spaces
//   3. Arrays of primitives: "key[N]: a, b, c" (inline, single line)
//   4. Arrays of uniform objects: tabular format (SCON's main size advantage over JSON)
//      - Header: "key[N]{field1,field2,...}:" written once
//      - Rows: " val1, val2, ..." × N (no repeated keys)
//      - Saves N×K key repetitions vs JSON's {"field1":v,"field2":v,...} per object
//      - Detection: O(R×K) scan verifies all items share identical keys with primitive values
//   5. Mixed arrays: expanded format with "- " prefix per item
//
// Quoting rules (two lookup tables, 256 bytes each, L1 cache resident):
//   - Values: quote if contains space, tab, colon, quotes, backslash, semicolon, @, #, braces, brackets
//   - Keys: same as values plus comma (comma is a delimiter in tabular headers)
//   - Strings matching "true"/"false"/"null" or starting with digit/+/-/. are always quoted
//     to prevent misinterpretation as primitives during decode

use crate::value::{Value, SconMap};
use crate::schema_registry::{SchemaRegistry, DefType};
use crate::treehash::TreeHash;

// Lookup tables — branch-free byte classification, L1 cache resident (256 bytes each)
// UNSAFE_VALUE[b] = true if byte b requires quoting in a SCON value
const UNSAFE_VALUE: [bool; 256] = {
    let mut t = [false; 256];
    t[b' ' as usize] = true;
    t[b'\t' as usize] = true;
    t[b':' as usize] = true;
    t[b'"' as usize] = true;
    t[b'\\' as usize] = true;
    t[b';' as usize] = true;
    t[b'@' as usize] = true;
    t[b'#' as usize] = true;
    t[b'{' as usize] = true;
    t[b'[' as usize] = true;
    t[b']' as usize] = true;
    t[b'}' as usize] = true;
    t
};

// UNSAFE_KEY[b] = true if byte b requires quoting in a SCON key (superset of value: adds comma)
const UNSAFE_KEY: [bool; 256] = {
    let mut t = [false; 256];
    t[b':' as usize] = true;
    t[b'[' as usize] = true;
    t[b']' as usize] = true;
    t[b'{' as usize] = true;
    t[b'}' as usize] = true;
    t[b'"' as usize] = true;
    t[b'\\' as usize] = true;
    t[b' ' as usize] = true;
    t[b'\t' as usize] = true;
    t[b';' as usize] = true;
    t[b'@' as usize] = true;
    t[b'#' as usize] = true;
    t[b',' as usize] = true;
    t
};

// P2.3: Pre-computed spaces for write_indent
const INDENT_SPACES: &str = "                                                                ";

pub struct Encoder {
    indent: usize,
    delimiter: char,
    auto_extract: bool,
    registry: SchemaRegistry,
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            indent: 1,
            delimiter: ',',
            auto_extract: false,
            registry: SchemaRegistry::new(),
        }
    }

    pub fn with_indent(mut self, indent: usize) -> Self {
        self.indent = indent.max(1);
        self
    }

    pub fn with_delimiter(mut self, delimiter: char) -> Self {
        self.delimiter = delimiter;
        self
    }

    pub fn with_auto_extract(mut self, enabled: bool) -> Self {
        self.auto_extract = enabled;
        self
    }

    pub fn with_schemas(mut self, schemas: Vec<(&str, Value)>) -> Self {
        for (name, def) in schemas {
            self.registry.register(DefType::Schema, name, def);
        }
        self
    }

    pub fn with_responses(mut self, responses: Vec<(&str, Value)>) -> Self {
        for (name, def) in responses {
            self.registry.register(DefType::Response, name, def);
        }
        self
    }

    pub fn with_security(mut self, security: Vec<(&str, Value)>) -> Self {
        for (name, def) in security {
            self.registry.register(DefType::Security, name, def);
        }
        self
    }

    pub fn encode(&mut self, data: &Value) -> String {
        let mut buf = String::with_capacity(1024);
        self.encode_to(data, &mut buf);
        buf
    }

    // P2.4: Write to external buffer — avoids allocation per call
    pub fn encode_to(&mut self, data: &Value, buf: &mut String) {
        // Auto-extract repeated schemas via TreeHash
        if self.auto_extract {
            if let Value::Object(_) | Value::Array(_) = data {
                self.detect_repeated_schemas(data);
            }
        }

        let has_defs = self.emit_definitions(buf);

        match data {
            Value::Object(obj) if obj.is_empty() => {
                if has_defs { buf.push('\n'); }
                buf.push_str("{}");
            }
            Value::Array(arr) if arr.is_empty() => {
                if has_defs { buf.push('\n'); }
                buf.push_str("[]");
            }
            _ => {
                if has_defs { buf.push('\n'); }
                self.encode_value(data, 0, buf);
            }
        }

        // Prune orphan schemas: defined but never referenced in the body
        if self.auto_extract {
            self.prune_orphan_schemas(buf);
        }
    }

    fn encode_value(&self, value: &Value, depth: usize, buf: &mut String) {
        match value {
            Value::Object(obj) => self.encode_object(obj, depth, buf),
            Value::Array(arr) => self.encode_array_value(None, arr, depth, buf),
            _ => {
                self.write_primitive(value, buf);
            }
        }
    }

    fn encode_object(&self, obj: &SconMap<compact_str::CompactString, Value>, depth: usize, buf: &mut String) {
        let mut first = true;
        for (key, val) in obj {
            if !first { buf.push('\n'); }
            first = false;

            match val {
                // Empty object
                Value::Object(inner) if inner.is_empty() => {
                    self.write_indent(depth, buf);
                    self.write_key(key, buf);
                    buf.push_str(": {}");
                }
                // Empty array
                Value::Array(arr) if arr.is_empty() => {
                    self.write_indent(depth, buf);
                    self.write_key(key, buf);
                    buf.push_str(": []");
                }
                // Primitive value
                v if v.is_primitive() => {
                    self.write_indent(depth, buf);
                    self.write_key(key, buf);
                    buf.push_str(": ");
                    self.write_primitive(v, buf);
                }
                // Array
                Value::Array(arr) => {
                    self.encode_array_value(Some(key), arr, depth, buf);
                }
                // Nested object — check for schema ref match
                Value::Object(inner) => {
                    if let Some(ref_name) = self.find_matching_schema(val) {
                        self.write_indent(depth, buf);
                        self.write_key(key, buf);
                        buf.push_str(": @s:");
                        buf.push_str(&ref_name);
                    } else {
                        self.write_indent(depth, buf);
                        self.write_key(key, buf);
                        buf.push(':');
                        buf.push('\n');
                        self.encode_object(inner, depth + 1, buf);
                    }
                }
                _ => {}
            }
        }
    }

    fn encode_array_value(&self, key: Option<&str>, arr: &[Value], depth: usize, buf: &mut String) {
        let len = arr.len();

        if len == 0 {
            self.write_indent(depth, buf);
            if let Some(k) = key {
                self.write_key(k, buf);
                buf.push_str(": []");
            } else {
                buf.push_str("[]");
            }
            return;
        }

        // Array of primitives → inline
        if arr.iter().all(|v| v.is_primitive()) {
            self.write_indent(depth, buf);
            if let Some(k) = key {
                self.write_key(k, buf);
            }
            buf.push('[');
            self.write_usize(len, buf);
            buf.push_str("]: ");
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    buf.push(self.delimiter);
                    buf.push(' ');
                }
                self.write_primitive(v, buf);
            }
            return;
        }

        // Array of objects with uniform keys → tabular
        if let Some(fields) = self.extract_tabular_fields(arr) {
            self.write_indent(depth, buf);
            if let Some(k) = key {
                self.write_key(k, buf);
            }
            buf.push('[');
            self.write_usize(len, buf);
            buf.push_str("]{");
            for (i, f) in fields.iter().enumerate() {
                if i > 0 { buf.push(self.delimiter); }
                self.write_key(f, buf);
            }
            buf.push_str("}:");
            for item in arr {
                if let Value::Object(obj) = item {
                    buf.push('\n');
                    self.write_indent(depth + 1, buf);
                    for (i, f) in fields.iter().enumerate() {
                        if i > 0 {
                            buf.push(self.delimiter);
                            buf.push(' ');
                        }
                        if let Some(v) = obj.get(*f) {
                            self.write_primitive(v, buf);
                        } else {
                            buf.push_str("null");
                        }
                    }
                }
            }
            return;
        }

        // Mixed / expanded array
        self.write_indent(depth, buf);
        if let Some(k) = key {
            self.write_key(k, buf);
        }
        buf.push('[');
        self.write_usize(len, buf);
        buf.push_str("]:");
        for item in arr {
            buf.push('\n');
            match item {
                v if v.is_primitive() => {
                    self.write_indent(depth + 1, buf);
                    buf.push_str("- ");
                    self.write_primitive(v, buf);
                }
                Value::Object(obj) if obj.is_empty() => {
                    self.write_indent(depth + 1, buf);
                    buf.push_str("- {}");
                }
                Value::Object(_) => {
                    if let Some(ref_name) = self.find_matching_schema(item) {
                        self.write_indent(depth + 1, buf);
                        buf.push_str("- @s:");
                        buf.push_str(&ref_name);
                    } else if let Value::Object(obj) = item {
                        self.encode_object_as_list_item(obj, depth + 1, buf);
                    }
                }
                Value::Array(inner) if inner.is_empty() => {
                    self.write_indent(depth + 1, buf);
                    buf.push_str("- []");
                }
                Value::Array(inner) if inner.iter().all(|v| v.is_primitive()) => {
                    self.write_indent(depth + 1, buf);
                    buf.push_str("- [");
                    self.write_usize(inner.len(), buf);
                    buf.push_str("]: ");
                    for (i, v) in inner.iter().enumerate() {
                        if i > 0 {
                            buf.push(self.delimiter);
                            buf.push(' ');
                        }
                        self.write_primitive(v, buf);
                    }
                }
                _ => {}
            }
        }
    }

    fn encode_object_as_list_item(&self, obj: &SconMap<compact_str::CompactString, Value>, depth: usize, buf: &mut String) {
        if obj.is_empty() {
            self.write_indent(depth, buf);
            buf.push_str("- ");
            return;
        }

        let mut iter = obj.iter();
        let (first_key, first_val) = iter.next().unwrap();

        self.write_indent(depth, buf);
        buf.push_str("- ");
        self.write_key(first_key, buf);

        match first_val {
            v if v.is_primitive() => {
                buf.push_str(": ");
                self.write_primitive(v, buf);
            }
            Value::Array(arr) if arr.is_empty() => {
                buf.push_str(": []");
            }
            Value::Array(arr) if arr.iter().all(|v| v.is_primitive()) => {
                buf.push('[');
                self.write_usize(arr.len(), buf);
                buf.push_str("]: ");
                for (i, v) in arr.iter().enumerate() {
                    if i > 0 {
                        buf.push(self.delimiter);
                        buf.push(' ');
                    }
                    self.write_primitive(v, buf);
                }
            }
            Value::Object(inner) if inner.is_empty() => {
                buf.push_str(": {}");
            }
            Value::Object(inner) => {
                buf.push(':');
                buf.push('\n');
                self.encode_object(inner, depth + 2, buf);
            }
            _ => {
                buf.push(':');
            }
        }

        // Continuation fields
        for (key, val) in iter {
            buf.push('\n');
            match val {
                v if v.is_primitive() => {
                    self.write_indent(depth + 1, buf);
                    self.write_key(key, buf);
                    buf.push_str(": ");
                    self.write_primitive(v, buf);
                }
                Value::Array(arr) if arr.is_empty() => {
                    self.write_indent(depth + 1, buf);
                    self.write_key(key, buf);
                    buf.push_str(": []");
                }
                Value::Array(arr) => {
                    self.encode_array_value(Some(key), arr, depth + 1, buf);
                }
                Value::Object(inner) if inner.is_empty() => {
                    self.write_indent(depth + 1, buf);
                    self.write_key(key, buf);
                    buf.push_str(": {}");
                }
                Value::Object(inner) => {
                    self.write_indent(depth + 1, buf);
                    self.write_key(key, buf);
                    buf.push(':');
                    buf.push('\n');
                    self.encode_object(inner, depth + 2, buf);
                }
                _ => {}
            }
        }
    }

    // Tabular detection: returns borrowed key refs if all items are objects with identical keys
    // and all values are primitive. Returns None → fall through to expanded format.
    // Borrows &str from IndexMap keys — zero allocation for the field list itself.
    fn extract_tabular_fields<'a>(&self, arr: &'a [Value]) -> Option<Vec<&'a str>> {
        if arr.is_empty() { return None; }

        let first = match &arr[0] {
            Value::Object(obj) if !obj.is_empty() => obj,
            _ => return None,
        };

        let keys: Vec<&str> = first.keys().map(|k| k.as_str()).collect();

        for v in first.values() {
            if !v.is_primitive() { return None; }
        }

        for item in &arr[1..] {
            match item {
                Value::Object(obj) => {
                    if obj.len() != keys.len() { return None; }
                    for k in &keys {
                        match obj.get(*k) {
                            Some(v) if v.is_primitive() => {}
                            _ => return None,
                        }
                    }
                }
                _ => return None,
            }
        }

        Some(keys)
    }

    // --- Primitive writing ---

    // P2.1: Write usize without temporary String allocation
    #[inline]
    fn write_usize(&self, n: usize, buf: &mut String) {
        let mut itoa_buf = itoa::Buffer::new();
        buf.push_str(itoa_buf.format(n));
    }

    fn write_primitive(&self, value: &Value, buf: &mut String) {
        match value {
            Value::Null => buf.push_str("null"),
            Value::Bool(true) => buf.push_str("true"),
            Value::Bool(false) => buf.push_str("false"),
            // P2.1: itoa — no temporary String for integers
            Value::Integer(n) => {
                let mut itoa_buf = itoa::Buffer::new();
                buf.push_str(itoa_buf.format(*n));
            }
            // P2.1: ryu — no temporary String for floats
            Value::Float(n) => {
                let mut ryu_buf = ryu::Buffer::new();
                buf.push_str(ryu_buf.format(*n));
            }
            Value::String(s) => self.write_string(s, buf),
            _ => {}
        }
    }

    fn write_string(&self, s: &str, buf: &mut String) {
        if self.is_safe_unquoted(s) {
            buf.push_str(s);
        } else {
            buf.push('"');
            self.escape_string(s, buf);
            buf.push('"');
        }
    }

    fn write_key(&self, key: &str, buf: &mut String) {
        if self.is_valid_unquoted_key(key) {
            buf.push_str(key);
        } else {
            buf.push('"');
            self.escape_string(key, buf);
            buf.push('"');
        }
    }

    // Chunk-based escape: flush clean segments in bulk, only handle escape chars individually.
    // SCON escapes: \\ \" \n \r \t \; (semicolon must be escaped — it's the minified delimiter)
    fn escape_string(&self, s: &str, buf: &mut String) {
        let bytes = s.as_bytes();
        let mut last_flush = 0;

        for (i, &b) in bytes.iter().enumerate() {
            let esc = match b {
                b'\\' => "\\\\",
                b'"'  => "\\\"",
                b'\n' => "\\n",
                b'\r' => "\\r",
                b'\t' => "\\t",
                b';'  => "\\;",
                _ => continue,
            };
            // Flush the clean segment before this escape
            if last_flush < i {
                buf.push_str(&s[last_flush..i]);
            }
            buf.push_str(esc);
            last_flush = i + 1;
        }
        // Flush remaining
        if last_flush < s.len() {
            buf.push_str(&s[last_flush..]);
        }
    }

    // Determines if a string value can be written without quotes.
    // Conservative: rejects anything starting with digit/+/-/. (could be parsed as number),
    // reserved words (true/false/null), and strings containing unsafe bytes.
    // False positives (quoting unnecessarily) are safe; false negatives would break decode.
    fn is_safe_unquoted(&self, s: &str) -> bool {
        if s.is_empty() { return false; }
        if matches!(s, "true" | "false" | "null") { return false; }
        // P6: Byte check — evita parse::<i64>()/parse::<f64>() que allocan en error path
        let first = s.as_bytes()[0];
        if first.is_ascii_digit() || first == b'+' || first == b'-' || first == b'.' {
            return false;
        }
        let delim_byte = self.delimiter as u8;
        for &b in s.as_bytes() {
            if UNSAFE_VALUE[b as usize] || b == delim_byte {
                return false;
            }
        }
        true
    }

    fn is_valid_unquoted_key(&self, key: &str) -> bool {
        if key.is_empty() { return false; }
        if key.as_bytes()[0] == b'#' { return false; }
        for &b in key.as_bytes() {
            if UNSAFE_KEY[b as usize] {
                return false;
            }
        }
        true
    }

    // P2.3: Use pre-computed slice instead of push loop
    fn write_indent(&self, depth: usize, buf: &mut String) {
        let spaces = self.indent * depth;
        if spaces == 0 { return; }
        if spaces <= INDENT_SPACES.len() {
            buf.push_str(&INDENT_SPACES[..spaces]);
        } else {
            // Fallback for very deep nesting
            let full = spaces / INDENT_SPACES.len();
            let rem = spaces % INDENT_SPACES.len();
            for _ in 0..full {
                buf.push_str(INDENT_SPACES);
            }
            buf.push_str(&INDENT_SPACES[..rem]);
        }
    }

    // --- Schema / autoExtract support ---

    fn detect_repeated_schemas(&mut self, data: &Value) {
        // normalize=false: key order from source (same as PHP default in encode path)
        let result = TreeHash::hash_tree(data, "", 2, false);
        for entry in result.index.values() {
            if entry.count >= 2 {
                let name = generate_schema_name(&entry.path);
                self.registry.register(DefType::Schema, &name, entry.data.clone());
            }
        }
    }

    // Emit s:/r:/sec: definitions at top of output. Returns true if any defs were emitted.
    fn emit_definitions(&self, buf: &mut String) -> bool {
        let mut emitted = false;

        let schemas = self.registry.get_all(DefType::Schema);
        if !schemas.is_empty() {
            for (name, def) in schemas {
                if emitted { buf.push('\n'); }
                buf.push_str("s:");
                buf.push_str(name);
                buf.push(' ');
                self.encode_inline(def, buf);
                emitted = true;
            }
        }

        let responses = self.registry.get_all(DefType::Response);
        if !responses.is_empty() {
            if emitted { buf.push('\n'); }
            for (name, def) in responses {
                if emitted { buf.push('\n'); }
                buf.push_str("r:");
                buf.push_str(name);
                buf.push(' ');
                self.encode_inline(def, buf);
                emitted = true;
            }
        }

        let security = self.registry.get_all(DefType::Security);
        if !security.is_empty() {
            if emitted { buf.push('\n'); }
            for (name, def) in security {
                if emitted { buf.push('\n'); }
                buf.push_str("sec:");
                buf.push_str(name);
                buf.push(' ');
                self.encode_inline(def, buf);
                emitted = true;
            }
        }

        emitted
    }

    // Single-line inline notation for schema definitions: {key:val, key2:val2}
    fn encode_inline(&self, data: &Value, buf: &mut String) {
        match data {
            Value::Null => buf.push_str("null"),
            Value::Bool(true) => buf.push_str("true"),
            Value::Bool(false) => buf.push_str("false"),
            Value::Integer(n) => {
                let mut b = itoa::Buffer::new();
                buf.push_str(b.format(*n));
            }
            Value::Float(n) => {
                let mut b = ryu::Buffer::new();
                buf.push_str(b.format(*n));
            }
            Value::String(s) => self.write_string(s, buf),
            Value::Array(arr) => {
                buf.push('[');
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 { buf.push_str(", "); }
                    self.encode_inline(item, buf);
                }
                buf.push(']');
            }
            Value::Object(obj) => {
                buf.push('{');
                for (i, (k, v)) in obj.iter().enumerate() {
                    if i > 0 { buf.push_str(", "); }
                    self.write_key(k, buf);
                    buf.push(':');
                    self.encode_inline(v, buf);
                }
                buf.push('}');
            }
        }
    }

    // Check if a Value matches a registered schema exactly
    fn find_matching_schema(&self, data: &Value) -> Option<String> {
        let schemas = self.registry.get_all(DefType::Schema);
        for (name, def) in schemas {
            if data == def {
                return Some(name.to_string());
            }
        }
        None
    }

    // Remove schema definitions that are never referenced in the output body
    fn prune_orphan_schemas(&self, buf: &mut String) {
        let schemas = self.registry.get_all(DefType::Schema);
        if schemas.is_empty() { return; }

        let mut orphans: Vec<String> = Vec::new();
        for name in schemas.keys() {
            let ref_marker = format!("@s:{}", name);
            // Check if the reference appears anywhere after the definitions section
            if !buf.contains(&ref_marker) {
                orphans.push(format!("s:{} ", name));
            }
        }

        if orphans.is_empty() { return; }

        // Rebuild buffer without orphan definition lines
        let lines: Vec<&str> = buf.lines().collect();
        let mut new_buf = String::with_capacity(buf.len());
        let mut first = true;
        for line in lines {
            if orphans.iter().any(|o| line.starts_with(o)) {
                continue;
            }
            if !first { new_buf.push('\n'); }
            new_buf.push_str(line);
            first = false;
        }

        // Remove leading empty lines from pruned defs
        let trimmed = new_buf.trim_start_matches('\n');
        *buf = trimmed.to_string();
    }
}

fn generate_schema_name(path: &str) -> String {
    let parts: Vec<&str> = path.trim_matches('.').split('.').collect();
    // Strip list indices ([0], [1]) from end
    let meaningful: Vec<&str> = parts.iter().copied()
        .rev()
        .skip_while(|p| p.starts_with('[') && p.ends_with(']') && p[1..p.len()-1].parse::<usize>().is_ok())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let last = meaningful.last().copied().unwrap_or("");
    // Clean common path segments
    let cleaned: String = last.replace("properties", "")
        .replace("content", "")
        .replace("application/json", "")
        .replace("schema", "")
        .trim_matches('.')
        .to_string();
    if cleaned.is_empty() {
        let hash = xxhash_rust::xxh3::xxh3_128(path.as_bytes());
        format!("auto_{:06x}", hash & 0xFFFFFF)
    } else {
        cleaned
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}
