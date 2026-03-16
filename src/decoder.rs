// src/decoder.rs
// SCON Decoder — SCON string → Value
//
// Architecture: two-pass parsing, O(L) time where L = input length.
//
// Pass 1 — Line classification:
//   Split input into lines, compute depth (leading spaces / indent), skip comments/directives.
//   Each line becomes a ParsedLine { depth, content: &str, has_bracket: bool }.
//   All content fields borrow from the input string (zero-copy).
//   has_bracket is a pre-filter: '[' before ':' marks potential array headers (~5% of lines).
//
// Pass 2 — Semantic interpretation:
//   Walk ParsedLine[] with a monotonic pointer. Each line is visited at most twice
//   (once for classification, once for value extraction). Functions return (result, next_index)
//   so callers advance without re-scanning children — O(N) total, not O(N×D).
//
//   Line types recognized:
//     "key: value"           → primitive key-value
//     "key:"                 → scope opener, children at depth+1
//     "key[N]: a, b, c"     → inline array of primitives
//     "key[N]{f1,f2,...}:"   → tabular array header, followed by N data rows
//     "key[N]:"              → expanded array, items prefixed with "- "
//     "- value"              → list item (primitive)
//     "- key: value"         → list item (object, first field inline, rest at depth+2)
//
// Key optimizations (see bench/README.md for measured impact):
//   - CompactString: inline ≤24 bytes, eliminates ~90% of heap allocs for keys/values
//   - Scratch buffer: shared String for unescape, capacity reused across all calls
//   - Fast-path unescape: memchr(b'\\') → if no backslashes, skip processing entirely
//   - memchr3 SIMD: single-pass ':', '"', '{' search in find_key_colon
//   - Manual integer parser: byte accumulator avoids stdlib FromStr overhead
//   - Inline delimited parsing: parse values during split (no intermediate Vec<&str>)
//
// Decode gap vs serde_json (1.6–1.8x) is architectural:
//   serde_json: single-pass recursive descent, zero-copy &'de str borrowing from input
//   SCON: two-pass (line array + semantic), CompactString copies (inline but still copies)

use crate::value::{Value, SconMap};
use crate::schema_registry::{SchemaRegistry, DefType};
use compact_str::CompactString;
use memchr::memchr;

pub struct Decoder {
    indent: usize,
    indent_auto_detect: bool,
    scratch: String, // Shared unescape buffer — capacity reused across calls, avoids per-string allocation
    registry: SchemaRegistry,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            indent: 1,
            indent_auto_detect: true,
            scratch: String::with_capacity(256),
            registry: SchemaRegistry::new(),
        }
    }

    pub fn with_indent(mut self, indent: usize) -> Self {
        self.indent = indent.max(1);
        self.indent_auto_detect = false;
        self
    }

    pub fn decode(&mut self, input: &str) -> Result<Value, String> {
        // P8: Decode minificado directo — evita materializar string expandido (~30% min-decode)
        if self.is_minified(input) {
            return self.decode_minified(input);
        }
        let scon: &str = input;

        //Auto-detect indent
        if self.indent_auto_detect {
            if let Some(cap) = scon.find('\n').and_then(|nl_pos| {
                let after = &scon[nl_pos + 1..];
                let spaces = after.len() - after.trim_start_matches(' ').len();
                if spaces > 0 && after.len() > spaces && !after.as_bytes().get(spaces).map_or(true, |b| b.is_ascii_whitespace()) {
                    Some(spaces)
                } else {
                    None
                }
            }) {
                self.indent = cap;
            } else {
                //Scan all lines for first indented non-empty line
                for line in scon.lines() {
                    let spaces = line.len() - line.trim_start_matches(' ').len();
                    if spaces > 0 && !line.trim().is_empty() && !line.trim().starts_with('#') {
                        self.indent = spaces;
                        break;
                    }
                }
            }
        }

        // Pre-alloc estimado — evita re-allocations del Vec durante parsing
        let line_estimate = scon.as_bytes().iter().filter(|&&b| b == b'\n').count() + 1;
        let mut parsed: Vec<ParsedLine<'_>> = Vec::with_capacity(line_estimate);
        let indent = self.indent;
        for (line_num, line) in scon.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            let first_byte = trimmed.as_bytes()[0];
            // Skip comments and directives — byte check antes de string compare
            if first_byte == b'#' { continue; }
            if first_byte == b'@' && (trimmed.starts_with("@@") || trimmed.starts_with("@use ")) { continue; }
            // Parse schema/response/security definitions into registry
            if first_byte == b's' && trimmed.starts_with("s:") {
                self.parse_schema_def(trimmed, "s");
                continue;
            }
            if first_byte == b'r' && trimmed.starts_with("r:") {
                self.parse_schema_def(trimmed, "r");
                continue;
            }
            if trimmed.starts_with("sec:") {
                self.parse_schema_def(trimmed, "sec");
                continue;
            }
            let spaces = line.len() - line.trim_start_matches(' ').len();
            let depth = if indent > 0 { spaces / indent } else { 0 };
            // Pre-filtro: '[' antes de ':' → candidato a array header (~5% de líneas)
            let has_bracket = memchr(b'[', trimmed.as_bytes())
                .map_or(false, |bp| memchr(b':', trimmed.as_bytes()).map_or(false, |cp| bp < cp));
            parsed.push(ParsedLine {
                depth,
                content: trimmed,
                _line_num: line_num,
                has_bracket,
            });
        }

        if parsed.is_empty() {
            return Ok(Value::Object(SconMap::default()));
        }

        let first = &parsed[0];

        //Empty object
        if parsed.len() == 1 && first.content == "{}" {
            return Ok(Value::Object(SconMap::default()));
        }

        //Array header at root
        if let Some(header) = self.try_array_header(first.content) {
            if header.key.is_none() {
                return self.decode_array_from_header(0, &parsed, &header).map(|(v, _)| v);
            }
        }

        //Single primitive
        if parsed.len() == 1 && !first.content.contains(':') {
            let val = self.parse_primitive(first.content);
            return Ok(val);
        }

        self.decode_object(0, &parsed, 0).map(|(obj, _)| Value::Object(obj))
    }

    // Direct minified decoder — parses `;`-delimited SCON without expanding to indented form.
    // Avoids allocating the full expanded String (~30% faster on minified input).
    // Depth tracking: ';' after scope opener increments, ';;'+ decrements by (count-1).
    fn decode_minified(&mut self, input: &str) -> Result<Value, String> {
        // Estimar segmentos por conteo de ';'
        let seg_estimate = input.as_bytes().iter().filter(|&&b| b == b';').count() + 1;
        let mut parsed: Vec<ParsedLine<'_>> = Vec::with_capacity(seg_estimate);
        let mut depth: usize = 0;
        let bytes = input.as_bytes();
        let mut seg_start = 0;
        let mut in_quotes = false;
        let mut line_num = 0;

        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];

            if c == b'\\' && in_quotes && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_quotes = !in_quotes;
                i += 1;
                continue;
            }

            if c == b';' && !in_quotes {
                let mut semi_count = 1usize;
                while i + 1 < bytes.len() && bytes[i + 1] == b';' {
                    semi_count += 1;
                    i += 1;
                }

                let segment = input[seg_start..i - semi_count + 1].trim();
                if !segment.is_empty() && !segment.starts_with('#') {
                    if segment.starts_with("@@") || segment.starts_with("@use ") {
                        // skip directives
                    } else if segment.starts_with("s:") {
                        self.parse_schema_def(segment, "s");
                    } else if segment.starts_with("r:") {
                        self.parse_schema_def(segment, "r");
                    } else if segment.starts_with("sec:") {
                        self.parse_schema_def(segment, "sec");
                    } else {
                        parsed.push(ParsedLine { depth, content: segment, _line_num: line_num, has_bracket: memchr(b'[', segment.as_bytes()).map_or(false, |bp| memchr(b':', segment.as_bytes()).map_or(false, |cp| bp < cp)) });
                        line_num += 1;

                        // Scope openers: key: → depth+1
                        if segment.ends_with(':') && !self.has_inline_value_after_colon(segment) {
                            depth += 1;
                        }
                        // List items: "- " → depth+1
                        if segment.starts_with("- ") {
                            depth += 1;
                        }
                    }
                }

                // Dedent por semicolons múltiples
                if semi_count >= 2 {
                    depth = depth.saturating_sub(semi_count - 1);
                }

                seg_start = i + 1;
                i += 1;
                continue;
            }

            i += 1;
        }

        // Último segmento
        let segment = input[seg_start..].trim();
        if !segment.is_empty() && !segment.starts_with('#') {
            if segment.starts_with("@@") || segment.starts_with("@use ") {
                // skip directives
            } else if segment.starts_with("s:") {
                self.parse_schema_def(segment, "s");
            } else if segment.starts_with("r:") {
                self.parse_schema_def(segment, "r");
            } else if segment.starts_with("sec:") {
                self.parse_schema_def(segment, "sec");
            } else {
                parsed.push(ParsedLine { depth, content: segment, _line_num: line_num, has_bracket: memchr(b'[', segment.as_bytes()).map_or(false, |bp| memchr(b':', segment.as_bytes()).map_or(false, |cp| bp < cp)) });
            }
        }

        if parsed.is_empty() {
            return Ok(Value::Object(SconMap::default()));
        }

        let first = &parsed[0];
        if parsed.len() == 1 && first.content == "{}" {
            return Ok(Value::Object(SconMap::default()));
        }
        if let Some(header) = self.try_array_header(first.content) {
            if header.key.is_none() {
                return self.decode_array_from_header(0, &parsed, &header).map(|(v, _)| v);
            }
        }
        if parsed.len() == 1 && !first.content.contains(':') {
            return Ok(self.parse_primitive(first.content));
        }

        self.decode_object(0, &parsed, 0).map(|(obj, _)| Value::Object(obj))
    }

    // Helper para decode_minified: determina si una línea tiene valor inline después del colon
    fn has_inline_value_after_colon(&self, s: &str) -> bool {
        if let Some(colon_pos) = self.find_key_colon(s) {
            let after = s[colon_pos + 1..].trim();
            !after.is_empty()
        } else {
            false
        }
    }

    fn is_minified(&self, s: &str) -> bool {
        !s.contains('\n') && s.contains(';')
    }

    // --- Object decoding ---

    // Returns (map, next_index) — callers advance to next_index without re-scanning children.
    // Before this pattern, parent loops did `while depth > base { i++ }` to skip children → O(N×D).
    // With next_index returns, each line is visited exactly once across all recursion → O(N).
    fn decode_object(&mut self, base_depth: usize, lines: &[ParsedLine<'_>], start: usize) -> Result<(SconMap<CompactString, Value>, usize), String> {
        let mut result = SconMap::default();
        let mut i = start;

        while i < lines.len() {
            let line = &lines[i];
            if line.depth < base_depth { break; }
            if line.depth > base_depth { i += 1; continue; }

            let content = line.content;

            // has_bracket pre-filtro: skip try_array_header para ~95% de líneas sin '['
            if line.has_bracket {
                if let Some(header) = self.try_array_header(content) {
                    if let Some(key) = header.key {
                        let (val, next_i) = self.decode_array_from_header(i, lines, &header)?;
                        result.insert(CompactString::from(key), val);
                        i = next_i;
                        continue;
                    }
                }
            }

            //Key-value
            if let Some(colon_pos) = self.find_key_colon(content) {
                let (key, value, next_i) = self.decode_key_value(line, lines, i, base_depth, colon_pos)?;
                result.insert(key, value);
                i = next_i;
                continue;
            }

            i += 1;
        }

        Ok((result, i))
    }

    // P5: colon_pos ya viene de find_key_colon — evita re-escanear en parse_key
    fn decode_key_value(&mut self, line: &ParsedLine<'_>, lines: &[ParsedLine<'_>], index: usize, base_depth: usize, colon_pos: usize) -> Result<(CompactString, Value, usize), String> {
        let content = line.content;
        let (key, key_end) = if content.as_bytes()[0] == b'"' {
            self.parse_key(content)?
        } else {
            // Fast path: colon_pos ya conocido, no re-escanear
            (CompactString::from(content[..colon_pos].trim()), colon_pos + 1)
        };
        let rest = content[key_end..].trim();

        if !rest.is_empty() {
            //Inline value: could be {}, [], or primitive
            let value = self.parse_inline_value(rest);
            return Ok((key, value, index + 1));
        }

        //Nested object/value
        if index + 1 < lines.len() && lines[index + 1].depth > base_depth {
            //Check if children are list items → array
            let child_depth = base_depth + 1;
            if index + 1 < lines.len() && lines[index + 1].depth == child_depth && lines[index + 1].content.starts_with("- ") {
                //It's an expanded array without header
                let mut items = Vec::new();
                let mut j = index + 1;
                while j < lines.len() && lines[j].depth >= child_depth {
                    if lines[j].depth == child_depth && lines[j].content.starts_with("- ") {
                        let item_content = &lines[j].content[2..];
                        if item_content.contains(':') {
                            let (obj, next_j) = self.decode_list_item_object(&lines[j], lines, j, base_depth)?;
                            items.push(Value::Object(obj));
                            j = next_j;
                            continue;
                        } else {
                            items.push(self.parse_inline_value(item_content));
                        }
                    }
                    j += 1;
                }
                return Ok((key, Value::Array(items), j));
            }

            // next_index retornado por decode_object — sin re-escaneo
            let (obj, next_i) = self.decode_object(base_depth + 1, lines, index + 1)?;
            return Ok((key, Value::Object(obj), next_i));
        }

        //Empty nested (key: with nothing after)
        Ok((key, Value::Object(SconMap::default()), index + 1))
    }

    // --- Array decoding ---

    // Retorna (value, next_index) — callers avanzan sin re-escaneo
    fn decode_array_from_header(&mut self, index: usize, lines: &[ParsedLine<'_>], header: &ArrayHeader<'_>) -> Result<(Value, usize), String> {
        if header.length == 0 {
            return Ok((Value::Array(vec![]), index + 1));
        }

        //Inline values
        if let Some(ref inline) = header.inline_values {
            if header.fields.is_none() {
                let values = self.parse_delimited_values(inline, header.delimiter);
                return Ok((Value::Array(values), index + 1));
            }
        }

        //Tabular
        if let Some(ref fields) = header.fields {
            return self.decode_tabular_array(index, lines, header.length, fields, header.delimiter);
        }

        //Expanded
        self.decode_expanded_array(index, lines, header.length)
    }

    fn decode_tabular_array(&mut self, header_idx: usize, lines: &[ParsedLine<'_>], expected: usize, fields: &[&str], delimiter: char) -> Result<(Value, usize), String> {
        let base_depth = lines[header_idx].depth;
        let mut result = Vec::with_capacity(expected);
        let mut i = header_idx + 1;

        while i < lines.len() && result.len() < expected {
            if lines[i].depth != base_depth + 1 { break; }
            let mut values = self.parse_delimited_values(lines[i].content, delimiter);
            // Pre-pad con Null si faltan campos
            values.resize(fields.len(), Value::Null);
            let mut row = SconMap::with_capacity_and_hasher(fields.len(), ahash::RandomState::new());
            // Consumir values sin clone — drain evita copia de cada Value
            for (field, val) in fields.iter().zip(values.drain(..)) {
                row.insert(CompactString::from(*field), val);
            }
            result.push(Value::Object(row));
            i += 1;
        }

        Ok((Value::Array(result), i))
    }

    fn decode_expanded_array(&mut self, header_idx: usize, lines: &[ParsedLine<'_>], expected: usize) -> Result<(Value, usize), String> {
        let base_depth = lines[header_idx].depth;
        let mut result = Vec::with_capacity(expected);
        let mut i = header_idx + 1;

        while i < lines.len() && result.len() < expected {
            let line = &lines[i];
            if line.depth != base_depth + 1 { break; }

            if line.content.starts_with("- ") {
                let item_content = &line.content[2..];

                if let Some(header) = self.try_array_header(item_content) {
                    if header.key.is_some() {
                        let (obj, next_i) = self.decode_list_item_object(line, lines, i, base_depth)?;
                        result.push(Value::Object(obj));
                        i = next_i;
                        continue;
                    } else if let Some(ref inline) = header.inline_values {
                        result.push(Value::Array(self.parse_delimited_values(inline, header.delimiter)));
                    }
                } else if item_content.contains(':') {
                    let (obj, next_i) = self.decode_list_item_object(line, lines, i, base_depth)?;
                    result.push(Value::Object(obj));
                    i = next_i;
                    continue;
                } else {
                    result.push(self.parse_inline_value(item_content));
                }
            }
            i += 1;
        }

        Ok((Value::Array(result), i))
    }

    // Retorna (map, next_index) — elimina re-escaneo en decode_expanded_array y decode_key_value
    fn decode_list_item_object(&mut self, _line: &ParsedLine<'_>, lines: &[ParsedLine<'_>], index: usize, base_depth: usize) -> Result<(SconMap<CompactString, Value>, usize), String> {
        let item_content = &lines[index].content[2..]; // skip "- "

        let mut result = SconMap::default();
        let cont_depth = base_depth + 2;
        let mut cont_start = index + 1;

        //First field may be an array header (e.g., "dependencies[1]: auth")
        if let Some(header) = self.try_array_header(item_content) {
            if let Some(key) = header.key {
                let (val, next_i) = self.decode_array_from_header(index, lines, &header)?;
                result.insert(CompactString::from(key), val);
                cont_start = next_i;
            }
        } else {
            let (key, key_end) = self.parse_key(item_content)?;
            let rest = item_content[key_end..].trim();

            if !rest.is_empty() {
                result.insert(key, self.parse_inline_value(rest));
            } else if index + 1 < lines.len() && lines[index + 1].depth >= cont_depth {
                let (obj, next_i) = self.decode_object(cont_depth, lines, index + 1)?;
                result.insert(key, Value::Object(obj));
                cont_start = next_i;
            } else {
                result.insert(key, Value::Object(SconMap::default()));
            }
        }

        //Continuation fields
        let mut i = cont_start;
        while i < lines.len() {
            let next = &lines[i];
            if next.depth < cont_depth { break; }
            if next.depth == cont_depth {
                if next.content.starts_with("- ") { break; }

                //Array header in continuation — has_bracket pre-filtro
                if next.has_bracket {
                    if let Some(header) = self.try_array_header(next.content) {
                        if let Some(k) = header.key {
                            let (val, next_i) = self.decode_array_from_header(i, lines, &header)?;
                            result.insert(CompactString::from(k), val);
                            i = next_i;
                            continue;
                        }
                    }
                }

                if let Some(colon_pos) = self.find_key_colon(next.content) {
                    let (k, v, next_i) = self.decode_key_value(next, lines, i, cont_depth, colon_pos)?;
                    result.insert(k, v);
                    i = next_i;
                    continue;
                }
            }
            i += 1;
        }

        Ok((result, i))
    }

    // --- Parsing helpers ---

    // Array header detection + parsing in one call. Quick reject: '[' must appear before ':'.
    // Recognizes: key[N]: vals, key[N]{f1,f2}: (tabular), key[N]: (expanded), [N]: vals (anonymous)
    fn try_array_header<'a>(&self, content: &'a str) -> Option<ArrayHeader<'a>> {
        let bracket_start = content.find('[')?;
        //Quick reject: bracket must appear before first colon
        let colon_pos = content.find(':')?;
        if bracket_start >= colon_pos { return None; }

        self.parse_array_header(content)
    }

    // Parses array header into borrowed slices from the original content — zero allocation.
    // Format: [key][length[delim]]{field1,field2,...}: [inline_values]
    // Supports custom delimiters: trailing \t in bracket = tab-delimited, | = pipe-delimited
    fn parse_array_header<'a>(&self, content: &'a str) -> Option<ArrayHeader<'a>> {
        let bracket_start = content.find('[')?;
        let key = if bracket_start > 0 {
            let raw = content[..bracket_start].trim();
            // Fast path: la mayoría de keys no tienen quotes
            if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
                Some(&raw[1..raw.len() - 1])
            } else {
                Some(raw)
            }
        } else {
            None
        };

        let bracket_end = content[bracket_start..].find(']').map(|p| p + bracket_start)?;
        let mut bracket_content = &content[bracket_start + 1..bracket_end];

        let mut delimiter = ',';
        if bracket_content.ends_with('\t') {
            delimiter = '\t';
            bracket_content = &bracket_content[..bracket_content.len() - 1];
        } else if bracket_content.ends_with('|') {
            delimiter = '|';
            bracket_content = &bracket_content[..bracket_content.len() - 1];
        }

        let length: usize = bracket_content.parse().unwrap_or(0);

        //Fields {field1,field2} — split_top_level ya retorna &str slices
        let mut fields = None;
        let after_bracket = &content[bracket_end + 1..];
        if after_bracket.starts_with('{') {
            if let Some(brace_end) = after_bracket.find('}') {
                let fields_str = &after_bracket[1..brace_end];
                let parts = self.split_top_level(fields_str, delimiter);
                fields = Some(parts.into_iter().map(|s| s.trim()).collect());
            }
        }

        //Inline values after : — slice del content, no copia
        let colon_pos = content.rfind(':')?;
        let after_colon = content[colon_pos + 1..].trim();
        let inline_values = if !after_colon.is_empty() {
            Some(after_colon)
        } else {
            None
        };

        Some(ArrayHeader { key, length, delimiter, fields, inline_values })
    }

    fn parse_key(&mut self, content: &str) -> Result<(CompactString, usize), String> {
        if content.starts_with('"') {
            let close = self.find_closing_quote(content, 0)
                .ok_or_else(|| "Unterminated quoted key".to_string())?;
            let key = CompactString::from(self.unescape_string(&content[1..close]));
            if close + 1 >= content.len() || content.as_bytes()[close + 1] != b':' {
                return Err("Missing colon after key".to_string());
            }
            Ok((key, close + 2))
        } else {
            let colon = content.find(':').ok_or_else(|| "Missing colon after key".to_string())?;
            let key = CompactString::from(content[..colon].trim());
            Ok((key, colon + 1))
        }
    }

    // memchr3 SIMD — un solo pase vectorizado busca ':', '"', '{' simultáneamente
    // Antes: memchr(:) + contains(") + contains({) = 3 escaneos sobre el mismo prefijo
    pub fn find_key_colon(&self, s: &str) -> Option<usize> {
        let bytes = s.as_bytes();

        // SIMD fast-path: memchr3 encuentra el primer ':', '"' o '{' en un solo pase
        if let Some(pos) = memchr::memchr3(b':', b'"', b'{', bytes) {
            // Si es ':' directo, es el key colon (caso común ~95% de líneas)
            if bytes[pos] == b':' {
                return Some(pos);
            }
            // Tiene '"' o '{' antes del ':' → fallback al scan manual desde pos
        } else {
            return None; // sin ':' en toda la línea
        }

        // Scan manual solo cuando hay quotes/braces (caso raro)
        let mut in_quotes = false;
        let mut brace_depth = 0i32;
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' && in_quotes && i + 1 < bytes.len() { i += 2; continue; }
            if c == b'"' { in_quotes = !in_quotes; i += 1; continue; }
            if !in_quotes {
                if c == b'{' { brace_depth += 1; }
                if c == b'}' { brace_depth -= 1; }
                if c == b':' && brace_depth == 0 { return Some(i); }
            }
            i += 1;
        }
        None
    }

    fn parse_inline_value(&mut self, input: &str) -> Value {
        let trimmed = input.trim();
        if trimmed.is_empty() { return Value::String(CompactString::const_new("")); }
        if trimmed == "[]" { return Value::Array(vec![]); }
        if trimmed == "{}" { return Value::Object(SconMap::default()); }

        //Inline object {key:val, ...}
        if trimmed.starts_with('{') {
            if let Some(inner) = self.extract_brace_content(trimmed) {
                return Value::Object(self.parse_inline_object(inner));
            }
        }

        //Inline array [a, b, c]
        if trimmed.starts_with('[') {
            if let Some(close) = self.find_matching_bracket(trimmed, 0) {
                let inner = &trimmed[1..close];
                let items = self.parse_delimited_values(inner, ',');
                return Value::Array(items);
            }
        }

        self.parse_primitive(trimmed)
    }

    fn parse_inline_object(&mut self, inner: &str) -> SconMap<CompactString, Value> {
        let mut result = SconMap::default();
        let parts = self.split_top_level(inner, ',');

        for part in &parts {
            let part = part.trim();
            if part.is_empty() { continue; }
            if let Some(colon) = self.find_key_colon(part) {
                let key = self.unquote_key(part[..colon].trim());
                let val = self.parse_inline_value(part[colon + 1..].trim());
                result.insert(key, val);
            }
        }

        result
    }

    // Inline split + parse in one pass — avoids intermediate Vec<&str> from split_top_level.
    // Respects nesting: delimiters inside quotes, braces, or brackets are not split points.
    fn parse_delimited_values(&mut self, input: &str, delimiter: char) -> Vec<Value> {
        let mut result = Vec::new();
        let mut seg_start = 0;
        let mut in_quotes = false;
        let mut brace_depth = 0i32;
        let mut bracket_depth = 0i32;
        let bytes = input.as_bytes();
        let delim_byte = delimiter as u8;

        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' && in_quotes && i + 1 < bytes.len() { i += 2; continue; }
            if c == b'"' { in_quotes = !in_quotes; }
            if !in_quotes {
                if c == b'{' { brace_depth += 1; }
                if c == b'}' { brace_depth -= 1; }
                if c == b'[' { bracket_depth += 1; }
                if c == b']' { bracket_depth -= 1; }
            }
            if c == delim_byte && !in_quotes && brace_depth == 0 && bracket_depth == 0 {
                result.push(self.parse_primitive(input[seg_start..i].trim()));
                seg_start = i + 1;
                i += 1;
                continue;
            }
            i += 1;
        }
        if seg_start < input.len() || !result.is_empty() {
            result.push(self.parse_primitive(input[seg_start..].trim()));
        }
        result
    }

    fn parse_primitive(&mut self, token: &str) -> Value {
        let t = token.trim();
        if t.is_empty() { return Value::String(CompactString::const_new("")); }
        if t == "[]" { return Value::Array(vec![]); }
        if t == "{}" { return Value::Object(SconMap::default()); }

        // Schema/response/security reference resolution
        if t.starts_with("@s:") || t.starts_with("@r:") || t.starts_with("@sec:") {
            return self.resolve_reference(t);
        }

        if t.starts_with('"') {
            if let Some(close) = self.find_closing_quote(t, 0) {
                return Value::String(CompactString::from(self.unescape_string(&t[1..close])));
            }
        }

        if t == "true" { return Value::Bool(true); }
        if t == "false" { return Value::Bool(false); }
        if t == "null" { return Value::Null; }

        // Parser numérico manual — acumulador byte-level para enteros, stdlib solo para floats
        // Evita overhead de parse::<i64>() (error allocation, Result branching)
        let first = t.as_bytes()[0];
        if first.is_ascii_digit() || first == b'+' || first == b'-' || first == b'.' {
            if let Some(val) = self.try_parse_number(t) {
                return val;
            }
        }

        Value::String(CompactString::from(t))
    }

    // Manual number parser — byte accumulator for integers, stdlib fallback for floats.
    // Integers (the common case): n = n*10 + digit with checked_mul overflow detection.
    // Avoids stdlib FromStr which allocates ParseIntError on failure.
    // Floats: only falls through to parse::<f64>() after confirming '.' or 'e'/'E' presence,
    // so the stdlib parse succeeds on first try (no error path allocation).
    fn try_parse_number(&self, t: &str) -> Option<Value> {
        let bytes = t.as_bytes();
        let len = bytes.len();
        let mut pos = 0;
        let neg = bytes[0] == b'-';
        if neg || bytes[0] == b'+' { pos += 1; }
        if pos >= len { return None; }

        // Empieza con dígito → integer o float
        if bytes[pos].is_ascii_digit() {
            let mut n: u64 = 0;
            let mut overflow = false;

            while pos < len && bytes[pos].is_ascii_digit() {
                let d = (bytes[pos] - b'0') as u64;
                match n.checked_mul(10).and_then(|v| v.checked_add(d)) {
                    Some(v) => n = v,
                    None => { overflow = true; break; }
                }
                pos += 1;
            }

            // Entero puro: todos los bytes consumidos, sin overflow
            if pos == len && !overflow {
                let val = if neg {
                    if n > (i64::MAX as u64) + 1 {
                        return t.parse::<f64>().ok().map(Value::Float);
                    }
                    if n == (i64::MAX as u64) + 1 { i64::MIN }
                    else { -(n as i64) }
                } else {
                    if n > i64::MAX as u64 {
                        return t.parse::<f64>().ok().map(Value::Float);
                    }
                    n as i64
                };
                return Some(Value::Integer(val));
            }

            // Punto decimal o exponente → float (stdlib, formato ya validado)
            if pos < len && (bytes[pos] == b'.' || bytes[pos] == b'e' || bytes[pos] == b'E') {
                return t.parse::<f64>().ok().map(Value::Float);
            }

            // Overflow sin punto/exp → float
            if overflow {
                return t.parse::<f64>().ok().map(Value::Float);
            }

            return None;
        }

        // Empieza con '.' → float
        if bytes[pos] == b'.' {
            return t.parse::<f64>().ok().map(Value::Float);
        }

        None
    }

    fn unquote_key(&mut self, s: &str) -> CompactString {
        let t = s.trim();
        if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
            CompactString::from(self.unescape_string(&t[1..t.len() - 1]))
        } else {
            CompactString::from(t)
        }
    }

    // P7: memchr2 SIMD — salta segmentos sin escapes ni quotes
    pub fn find_closing_quote(&self, s: &str, start: usize) -> Option<usize> {
        let bytes = s.as_bytes();
        let mut i = start + 1;
        while i < bytes.len() {
            match memchr::memchr2(b'\\', b'"', &bytes[i..]) {
                Some(offset) => {
                    let pos = i + offset;
                    if bytes[pos] == b'"' { return Some(pos); }
                    // Es backslash — saltar el escape
                    i = pos + 2;
                }
                None => return None,
            }
        }
        None
    }

    // Fast-path + scratch buffer — dos optimizaciones combinadas:
    // 1. Si no hay backslashes, retorna slice directo sin procesar (mayoría de strings)
    // 2. Si hay escapes, usa scratch buffer compartido (capacidad reutilizada entre llamadas)
    fn unescape_string(&mut self, s: &str) -> String {
        // Fast path: sin backslashes → copia directa, skip byte-by-byte loop
        if memchr(b'\\', s.as_bytes()).is_none() {
            return s.to_string();
        }
        // Slow path: chunk-copy por memchr — copia segmentos limpios de golpe, solo procesa escapes
        self.scratch.clear();
        let mut i = 0;
        while i < s.len() {
            match memchr(b'\\', &s.as_bytes()[i..]) {
                Some(offset) => {
                    // Flush chunk limpio antes del escape
                    if offset > 0 {
                        self.scratch.push_str(&s[i..i + offset]);
                    }
                    let esc_pos = i + offset;
                    if esc_pos + 1 < s.len() {
                        match s.as_bytes()[esc_pos + 1] {
                            b'\\' => self.scratch.push('\\'),
                            b'"' => self.scratch.push('"'),
                            b'n' => self.scratch.push('\n'),
                            b'r' => self.scratch.push('\r'),
                            b't' => self.scratch.push('\t'),
                            b';' => self.scratch.push(';'),
                            other => {
                                self.scratch.push('\\');
                                self.scratch.push(other as char);
                            }
                        }
                        i = esc_pos + 2;
                    } else {
                        self.scratch.push('\\');
                        i = esc_pos + 1;
                    }
                }
                None => {
                    // Sin más escapes — flush todo el resto de golpe
                    self.scratch.push_str(&s[i..]);
                    break;
                }
            }
        }
        self.scratch.clone()
    }

    //P1.3: Returns &str slice instead of String — zero-copy
    pub fn extract_brace_content<'a>(&self, input: &'a str) -> Option<&'a str> {
        let mut depth = 0i32;
        let mut start = None;
        let mut in_quotes = false;
        let bytes = input.as_bytes();

        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' && in_quotes && i + 1 < bytes.len() { i += 2; continue; }
            if c == b'"' { in_quotes = !in_quotes; i += 1; continue; }
            if !in_quotes {
                if c == b'{' {
                    if depth == 0 { start = Some(i); }
                    depth += 1;
                }
                if c == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        let s = start.unwrap_or(0);
                        return Some(&input[s + 1..i]);
                    }
                }
            }
            i += 1;
        }
        None
    }

    pub fn find_matching_bracket(&self, s: &str, start: usize) -> Option<usize> {
        let mut depth = 0i32;
        let mut in_quotes = false;
        let bytes = s.as_bytes();

        let mut i = start;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' && in_quotes && i + 1 < bytes.len() { i += 2; continue; }
            if c == b'"' { in_quotes = !in_quotes; i += 1; continue; }
            if !in_quotes {
                if c == b'[' { depth += 1; }
                if c == b']' {
                    depth -= 1;
                    if depth == 0 { return Some(i); }
                }
            }
            i += 1;
        }
        None
    }

    //P1.2: Returns Vec<&str> slices instead of Vec<String> — zero-copy
    pub fn split_top_level<'a>(&self, input: &'a str, delimiter: char) -> Vec<&'a str> {
        let mut parts = Vec::new();
        let mut seg_start = 0;
        let mut in_quotes = false;
        let mut brace_depth = 0i32;
        let mut bracket_depth = 0i32;
        let bytes = input.as_bytes();
        let delim_byte = delimiter as u8;

        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' && in_quotes && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == b'"' { in_quotes = !in_quotes; }
            if !in_quotes {
                if c == b'{' { brace_depth += 1; }
                if c == b'}' { brace_depth -= 1; }
                if c == b'[' { bracket_depth += 1; }
                if c == b']' { bracket_depth -= 1; }
            }
            if c == delim_byte && !in_quotes && brace_depth == 0 && bracket_depth == 0 {
                parts.push(&input[seg_start..i]);
                seg_start = i + 1;
                i += 1;
                continue;
            }
            i += 1;
        }
        if seg_start < input.len() || !parts.is_empty() {
            parts.push(&input[seg_start..]);
        }
        parts
    }

    // --- Schema definition parsing and reference resolution ---

    // Parse "s:name {inline}" or "r:name {inline}" or "sec:name {inline}"
    pub fn parse_schema_def(&mut self, line: &str, prefix: &str) {
        let after_prefix = &line[prefix.len() + 1..]; // skip "s:" or "r:" or "sec:"
        let after_prefix = after_prefix.trim();
        // Split name from definition: first non-space token is name
        let name_end = after_prefix.find(|c: char| c.is_whitespace()).unwrap_or(after_prefix.len());
        let name = &after_prefix[..name_end];
        let def_str = after_prefix[name_end..].trim();

        let def = if def_str.is_empty() {
            Value::Object(SconMap::default())
        } else {
            self.parse_inline_value(def_str)
        };

        if let Some(dt) = DefType::from_prefix(prefix) {
            self.registry.register(dt, name, def);
        }
    }

    // Resolve @type:name [overrides] or @s:a | @s:b (polymorphic)
    pub fn resolve_reference(&mut self, ref_str: &str) -> Value {
        // Polymorphic: @s:a | @s:b
        if ref_str.contains(" | ") {
            let mut schemas = Vec::new();
            for part in ref_str.split(" | ") {
                let part = part.trim();
                if let Some((dt, name)) = self.parse_ref_token(part) {
                    if let Ok(resolved) = self.registry.resolve(dt, name) {
                        schemas.push(resolved);
                    }
                }
            }
            let mut result = SconMap::default();
            result.insert(CompactString::from("oneOf"), Value::Array(schemas));
            return Value::Object(result);
        }

        // Simple or with overrides: @s:name {overrides}
        let (ref_part, override_str) = if let Some(brace_pos) = ref_str.find('{') {
            (&ref_str[..brace_pos].trim_end(), Some(&ref_str[brace_pos..]))
        } else {
            (&ref_str.trim_end(), None)
        };

        if let Some((dt, name)) = self.parse_ref_token(ref_part) {
            if let Some(ovr_str) = override_str {
                let overrides = self.parse_inline_value(ovr_str);
                if let Ok(resolved) = self.registry.resolve_with_override(dt, name, &overrides) {
                    return resolved;
                }
            } else if let Ok(resolved) = self.registry.resolve(dt, name) {
                return resolved;
            }
        }

        // Unresolvable reference — return as string
        Value::String(CompactString::from(ref_str))
    }

    // Parse @type:name → (DefType, name)
    fn parse_ref_token<'b>(&self, token: &'b str) -> Option<(DefType, &'b str)> {
        let token = token.trim();
        if !token.starts_with('@') { return None; }
        let rest = &token[1..];
        if let Some(colon_pos) = rest.find(':') {
            let prefix = &rest[..colon_pos];
            let name = rest[colon_pos + 1..].trim();
            DefType::from_prefix(prefix).map(|dt| (dt, name))
        } else {
            None
        }
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

// Zero-copy line representation — content borrows from input string, no allocation per line.
// 32 bytes per entry (usize + &str + usize + bool + padding).
// The Vec<ParsedLine> is the only allocation in pass 1.
struct ParsedLine<'a> {
    depth: usize,       // Nesting level = leading_spaces / indent
    content: &'a str,   // Trimmed line content, borrowed from input
    _line_num: usize,   // Original line number (for error reporting)
    has_bracket: bool,   // '[' before ':' → candidate for array header (~5% of lines are true)
}

// Array header parsed from "key[N]{f1,f2,...}: values" — all fields borrow from input.
// Supports three array formats:
//   Inline:   key[3]: a, b, c           (fields=None, inline_values=Some)
//   Tabular:  key[N]{f1,f2,...}:         (fields=Some, inline_values=None, followed by N rows)
//   Expanded: key[N]:                    (fields=None, inline_values=None, followed by "- " items)
struct ArrayHeader<'a> {
    key: Option<&'a str>,
    length: usize,
    delimiter: char,
    fields: Option<Vec<&'a str>>,
    inline_values: Option<&'a str>,
}
