// src/tape.rs
// SCON Tape Decoder — single-pass, parses SCON into a flat Vec<Node> (no tree allocation).
//
// Instead of building nested IndexMap + Vec structures, the tape is a single contiguous
// Vec<Node> where each node is a tagged union. Objects/Arrays store their child count,
// enabling O(1) skip-over without parsing children.
//
// Architecture: single-pass with 1-line lookahead via LineIter + ScopeInfo depth stack.
// No intermediate Vec<ParsedLine> — each line is parsed and emitted immediately.
//
// Memory layout comparison (for an object with 3 string fields):
//   Owned:    1 IndexMap + 3 CompactString keys + 3 CompactString values = ~7 allocations
//   Borrowed: 1 IndexMap + 0 string copies = ~1 allocation (the map itself)
//   Tape:     7 entries in a pre-allocated Vec = 0 additional allocations
//
// Trade-off: O(K) linear scan for key lookup (no hash table), but zero per-node allocation.
// Ideal for parse-and-forward, serialization, or full-tree traversal.
// Not ideal for random key access on large objects.

use crate::decoder::Decoder;
use memchr::memchr;

// 32 bytes per node on 64-bit (tag + payload + &str pointer + len)
#[derive(Debug, Clone, PartialEq)]
pub enum Node<'a> {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(&'a str),          // borrowed from input
    // Object: count = number of key-value pairs. Keys follow as String nodes.
    // Layout: Object(3) → Key, Value, Key, Value, Key, Value
    Object(usize),
    // Array: count = number of elements
    Array(usize),
    // Key in an object — always followed by its value node
    Key(&'a str),
}

pub struct Tape<'a> {
    pub nodes: Vec<Node<'a>>,
    // Backing store for strings from resolved schema references (@s:name).
    // Node::String/Key can point into these — heap buffer is stable across Vec reallocs.
    _resolved: Vec<String>,
}

impl<'a> Tape<'a> {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

// Single-pass line iterator with cached peek — no intermediate Vec<ParsedLine>
// Peek is cached so repeated peek_line() calls are O(1) after the first
struct LineIter<'a> {
    remaining: &'a str,
    indent: usize,
    // Cached peek: (depth, content, has_bracket, remaining_after)
    peeked: Option<(usize, &'a str, bool, &'a str)>,
}

impl<'a> LineIter<'a> {
    fn new(input: &'a str, indent_hint: usize) -> Self {
        Self { remaining: input, indent: indent_hint, peeked: None }
    }

    fn auto_detect_indent(&mut self, input: &'a str) {
        if let Some(nl) = input.find('\n') {
            let after = &input[nl + 1..];
            let spaces = after.len() - after.trim_start_matches(' ').len();
            if spaces > 0 && after.len() > spaces {
                let byte_after = after.as_bytes()[spaces];
                if !byte_after.is_ascii_whitespace() {
                    self.indent = spaces;
                    return;
                }
            }
        }
        for line in input.lines() {
            let spaces = line.len() - line.trim_start_matches(' ').len();
            if spaces > 0 && !line.trim().is_empty() && !line.trim().starts_with('#') {
                self.indent = spaces;
                return;
            }
        }
    }

    // Scan forward from `from` to find next meaningful line
    fn scan_next(from: &'a str, indent: usize) -> Option<(usize, &'a str, bool, &'a str)> {
        let mut remaining = from;
        loop {
            if remaining.is_empty() { return None; }
            let (line, rest) = match memchr(b'\n', remaining.as_bytes()) {
                Some(pos) => (&remaining[..pos], &remaining[pos + 1..]),
                None => (remaining, &remaining[remaining.len()..]),
            };
            remaining = rest;

            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            let first = trimmed.as_bytes()[0];
            if first == b'#' { continue; }
            if first == b'@' && (trimmed.starts_with("@@") || trimmed.starts_with("@use ")) { continue; }
            if first == b's' && trimmed.starts_with("s:") { continue; }
            if first == b'r' && trimmed.starts_with("r:") { continue; }
            if trimmed.starts_with("sec:") { continue; }

            let spaces = line.len() - line.trim_start_matches(' ').len();
            let depth = if indent > 0 { spaces / indent } else { 0 };
            let has_bracket = memchr(b'[', trimmed.as_bytes())
                .map_or(false, |bp| memchr(b':', trimmed.as_bytes()).map_or(false, |cp| bp < cp));

            return Some((depth, trimmed, has_bracket, remaining));
        }
    }

    fn next_line(&mut self) -> Option<(usize, &'a str, bool)> {
        if let Some((depth, content, has_bracket, rest)) = self.peeked.take() {
            self.remaining = rest;
            return Some((depth, content, has_bracket));
        }
        if let Some((depth, content, has_bracket, rest)) = Self::scan_next(self.remaining, self.indent) {
            self.remaining = rest;
            Some((depth, content, has_bracket))
        } else {
            None
        }
    }

    fn peek_line(&mut self) -> Option<(usize, &'a str, bool)> {
        if let Some((d, c, h, _)) = self.peeked {
            return Some((d, c, h));
        }
        if let Some(result) = Self::scan_next(self.remaining, self.indent) {
            self.peeked = Some(result);
            Some((result.0, result.1, result.2))
        } else {
            None
        }
    }
}

struct ArrayHeader<'a> {
    key: Option<&'a str>,
    length: usize,
    delimiter: char,
    fields: Option<Vec<&'a str>>,
    inline_values: Option<&'a str>,
}

pub struct TapeDecoder {
    helper: Decoder,
    indent: usize,
    indent_auto_detect: bool,
    // Accumulates owned strings from resolved schema refs during decode;
    // transferred to Tape._resolved so Node<'a> refs remain valid.
    resolved_strs: Vec<String>,
}

impl TapeDecoder {
    pub fn new() -> Self {
        Self {
            helper: Decoder::new(),
            indent: 1,
            indent_auto_detect: true,
            resolved_strs: Vec::new(),
        }
    }

    pub fn decode<'a>(&mut self, input: &'a str) -> Result<Tape<'a>, String> {
        self.resolved_strs.clear();

        if !input.contains('\n') && input.contains(';') {
            // Minified — delegate to owned decoder (which handles schema defs)
            let val = Decoder::new().decode(input)?;
            return Ok(self.owned_to_tape(&val));
        }

        self.prescan_schema_defs(input);

        let mut lines = LineIter::new(input, self.indent);
        if self.indent_auto_detect {
            lines.auto_detect_indent(input);
            self.indent = lines.indent;
        }

        let node_estimate = (input.len() / 15).max(16);
        let mut tape: Vec<Node<'a>> = Vec::with_capacity(node_estimate);

        let first = match lines.peek_line() {
            Some(f) => f,
            None => {
                tape.push(Node::Object(0));
                return Ok(Tape { nodes: tape, _resolved: std::mem::take(&mut self.resolved_strs) });
            }
        };

        if first.1 == "{}" {
            lines.next_line();
            if lines.peek_line().is_none() {
                tape.push(Node::Object(0));
                return Ok(Tape { nodes: tape, _resolved: std::mem::take(&mut self.resolved_strs) });
            }
        }

        if let Some(header) = self.try_array_header(first.1) {
            if header.key.is_none() {
                lines.next_line();
                self.emit_array_from_header_streaming(&header, &mut lines, first.0, &mut tape)?;
                return Ok(Tape { nodes: tape, _resolved: std::mem::take(&mut self.resolved_strs) });
            }
        }

        if lines.peek_line().map_or(false, |(_, c, _)| !c.contains(':')) {
            let (_, content, _) = lines.next_line().unwrap();
            if lines.peek_line().is_none() {
                self.emit_primitive(content, &mut tape);
                return Ok(Tape { nodes: tape, _resolved: std::mem::take(&mut self.resolved_strs) });
            }
            self.emit_primitive(content, &mut tape);
            tape.clear();
            lines = LineIter::new(input, self.indent);
            if self.indent_auto_detect {
                lines.auto_detect_indent(input);
            }
        }

        self.emit_object_streaming(&mut lines, 0, &mut tape)?;

        Ok(Tape { nodes: tape, _resolved: std::mem::take(&mut self.resolved_strs) })
    }

    // --- Single-pass object emission with stack-based depth tracking ---

    fn emit_object_streaming<'a>(&mut self, lines: &mut LineIter<'a>, base_depth: usize, tape: &mut Vec<Node<'a>>) -> Result<(), String> {
        let obj_pos = tape.len();
        tape.push(Node::Object(0));
        let mut count = 0usize;

        loop {
            let (depth, content, has_bracket) = match lines.peek_line() {
                Some(l) => l,
                None => break,
            };

            if depth < base_depth { break; }
            if depth > base_depth {
                // skip lines that belong to deeper scopes (handled by recursive calls)
                lines.next_line();
                continue;
            }

            // Consume the line
            lines.next_line();

            // Array header check
            if has_bracket {
                if let Some(header) = self.try_array_header(content) {
                    if let Some(key) = header.key {
                        tape.push(Node::Key(key));
                        self.emit_array_from_header_streaming(&header, lines, base_depth, tape)?;
                        count += 1;
                        continue;
                    }
                }
            }

            // Key-value pair
            if let Some(colon_pos) = self.helper.find_key_colon(content) {
                self.emit_key_value_streaming(content, colon_pos, lines, base_depth, tape)?;
                count += 1;
                continue;
            }

            // unknown line at base depth — skip
        }

        tape[obj_pos] = Node::Object(count);
        Ok(())
    }

    fn emit_key_value_streaming<'a>(&mut self, content: &'a str, colon_pos: usize, lines: &mut LineIter<'a>, base_depth: usize, tape: &mut Vec<Node<'a>>) -> Result<(), String> {
        let (key, key_end) = if content.as_bytes()[0] == b'"' {
            self.parse_key(content)?
        } else {
            (content[..colon_pos].trim(), colon_pos + 1)
        };
        tape.push(Node::Key(key));
        let rest = content[key_end..].trim();

        if !rest.is_empty() {
            self.emit_inline_value(rest, tape);
            return Ok(());
        }

        // Peek next line to determine child type — the 1-line lookahead
        if let Some((next_depth, next_content, _next_has_bracket)) = lines.peek_line() {
            if next_depth > base_depth {
                let child_depth = base_depth + 1;
                if next_depth == child_depth && next_content.starts_with("- ") {
                    // Expanded array (list items)
                    self.emit_expanded_list_streaming(lines, child_depth, base_depth, tape)?;
                    return Ok(());
                }
                // Nested object
                self.emit_object_streaming(lines, child_depth, tape)?;
                return Ok(());
            }
        }

        // No children — empty object
        tape.push(Node::Object(0));
        Ok(())
    }

    // --- Array emission ---

    fn emit_array_from_header_streaming<'a>(&mut self, header: &ArrayHeader<'a>, lines: &mut LineIter<'a>, base_depth: usize, tape: &mut Vec<Node<'a>>) -> Result<(), String> {
        if header.length == 0 {
            tape.push(Node::Array(0));
            return Ok(());
        }

        if let Some(ref inline) = header.inline_values {
            if header.fields.is_none() {
                let arr_pos = tape.len();
                tape.push(Node::Array(0));
                let count = self.emit_delimited_values(inline, header.delimiter, tape);
                tape[arr_pos] = Node::Array(count);
                return Ok(());
            }
        }

        if let Some(ref fields) = header.fields {
            return self.emit_tabular_array_streaming(lines, base_depth, header.length, fields, header.delimiter, tape);
        }

        self.emit_expanded_array_streaming(lines, base_depth, header.length, tape)
    }

    fn emit_tabular_array_streaming<'a>(&mut self, lines: &mut LineIter<'a>, base_depth: usize, expected: usize, fields: &[&'a str], delimiter: char, tape: &mut Vec<Node<'a>>) -> Result<(), String> {
        let arr_pos = tape.len();
        tape.push(Node::Array(0));
        let mut row_count = 0;

        while row_count < expected {
            let (depth, content, _) = match lines.peek_line() {
                Some(l) => l,
                None => break,
            };
            if depth != base_depth + 1 { break; }
            lines.next_line();

            let obj_pos = tape.len();
            tape.push(Node::Object(fields.len()));
            let mut seg_start = 0;
            let mut in_quotes = false;
            let bytes = content.as_bytes();
            let delim_byte = delimiter as u8;
            let mut field_idx = 0;

            let mut j = 0;
            while j < bytes.len() {
                let c = bytes[j];
                if c == b'\\' && in_quotes && j + 1 < bytes.len() { j += 2; continue; }
                if c == b'"' { in_quotes = !in_quotes; }
                if c == delim_byte && !in_quotes {
                    if field_idx < fields.len() {
                        tape.push(Node::Key(fields[field_idx]));
                        self.emit_primitive(content[seg_start..j].trim(), tape);
                    }
                    field_idx += 1;
                    seg_start = j + 1;
                    j += 1;
                    continue;
                }
                j += 1;
            }
            if field_idx < fields.len() {
                tape.push(Node::Key(fields[field_idx]));
                self.emit_primitive(content[seg_start..].trim(), tape);
                field_idx += 1;
            }
            while field_idx < fields.len() {
                tape.push(Node::Key(fields[field_idx]));
                tape.push(Node::Null);
                field_idx += 1;
            }
            tape[obj_pos] = Node::Object(fields.len());
            row_count += 1;
        }

        tape[arr_pos] = Node::Array(row_count);
        Ok(())
    }

    fn emit_expanded_array_streaming<'a>(&mut self, lines: &mut LineIter<'a>, base_depth: usize, expected: usize, tape: &mut Vec<Node<'a>>) -> Result<(), String> {
        let arr_pos = tape.len();
        tape.push(Node::Array(0));
        let mut count = 0;
        let child_depth = base_depth + 1;

        while count < expected {
            let (depth, content, _) = match lines.peek_line() {
                Some(l) => l,
                None => break,
            };
            if depth != child_depth { break; }

            if content.starts_with("- ") {
                lines.next_line();
                let item_content = &content[2..];

                if let Some(header) = self.try_array_header(item_content) {
                    if header.key.is_some() {
                        self.emit_list_item_object_streaming(item_content, lines, base_depth, tape)?;
                        count += 1;
                        continue;
                    } else if let Some(ref inline) = header.inline_values {
                        let sub_pos = tape.len();
                        tape.push(Node::Array(0));
                        let sub_count = self.emit_delimited_values(inline, header.delimiter, tape);
                        tape[sub_pos] = Node::Array(sub_count);
                        count += 1;
                        continue;
                    }
                } else if item_content.contains(':') {
                    self.emit_list_item_object_streaming(item_content, lines, base_depth, tape)?;
                    count += 1;
                    continue;
                } else {
                    self.emit_inline_value(item_content, tape);
                    count += 1;
                    continue;
                }
            }
            lines.next_line();
        }

        tape[arr_pos] = Node::Array(count);
        Ok(())
    }

    // Expanded list without header (e.g. `key:\n  - item1\n  - item2`)
    fn emit_expanded_list_streaming<'a>(&mut self, lines: &mut LineIter<'a>, child_depth: usize, base_depth: usize, tape: &mut Vec<Node<'a>>) -> Result<(), String> {
        let arr_pos = tape.len();
        tape.push(Node::Array(0));
        let mut count = 0;

        loop {
            let (depth, content, _) = match lines.peek_line() {
                Some(l) => l,
                None => break,
            };
            if depth < child_depth { break; }
            if depth > child_depth {
                // belongs to a deeper scope, consumed by recursive calls
                lines.next_line();
                continue;
            }
            if depth == child_depth && !content.starts_with("- ") { break; }

            lines.next_line();
            let item_content = &content[2..];

            if item_content.contains(':') {
                self.emit_list_item_object_streaming(item_content, lines, base_depth, tape)?;
                count += 1;
            } else {
                self.emit_inline_value(item_content, tape);
                count += 1;
            }
        }

        tape[arr_pos] = Node::Array(count);
        Ok(())
    }

    fn emit_list_item_object_streaming<'a>(&mut self, item_content: &'a str, lines: &mut LineIter<'a>, base_depth: usize, tape: &mut Vec<Node<'a>>) -> Result<(), String> {
        let obj_pos = tape.len();
        tape.push(Node::Object(0));
        let cont_depth = base_depth + 2;
        let mut count = 0;

        // Parse the first key-value from the "- key: value" line
        if let Some(header) = self.try_array_header(item_content) {
            if let Some(key) = header.key {
                tape.push(Node::Key(key));
                self.emit_array_from_header_streaming(&header, lines, cont_depth.wrapping_sub(1), tape)?;
                count += 1;
            }
        } else {
            let (key, key_end) = self.parse_key(item_content)?;
            tape.push(Node::Key(key));
            let rest = item_content[key_end..].trim();

            if !rest.is_empty() {
                self.emit_inline_value(rest, tape);
                count += 1;
            } else {
                // Peek to see if next line is a child object
                if let Some((next_depth, _, _)) = lines.peek_line() {
                    if next_depth >= cont_depth {
                        self.emit_object_streaming(lines, cont_depth, tape)?;
                        count += 1;
                    } else {
                        tape.push(Node::Object(0));
                        count += 1;
                    }
                } else {
                    tape.push(Node::Object(0));
                    count += 1;
                }
            }
        }

        // Continuation fields at cont_depth
        loop {
            let (depth, content, has_bracket) = match lines.peek_line() {
                Some(l) => l,
                None => break,
            };
            if depth < cont_depth { break; }
            if depth > cont_depth {
                lines.next_line();
                continue;
            }
            // depth == cont_depth
            if content.starts_with("- ") { break; }

            lines.next_line();

            if has_bracket {
                if let Some(header) = self.try_array_header(content) {
                    if let Some(k) = header.key {
                        tape.push(Node::Key(k));
                        self.emit_array_from_header_streaming(&header, lines, cont_depth, tape)?;
                        count += 1;
                        continue;
                    }
                }
            }

            if let Some(colon_pos) = self.helper.find_key_colon(content) {
                self.emit_key_value_streaming(content, colon_pos, lines, cont_depth, tape)?;
                count += 1;
                continue;
            }
        }

        tape[obj_pos] = Node::Object(count);
        Ok(())
    }

    // --- Parsing helpers ---

    fn try_array_header<'a>(&self, content: &'a str) -> Option<ArrayHeader<'a>> {
        let bracket_start = content.find('[')?;
        let colon_pos = content.find(':')?;
        if bracket_start >= colon_pos { return None; }
        self.parse_array_header(content)
    }

    fn parse_array_header<'a>(&self, content: &'a str) -> Option<ArrayHeader<'a>> {
        let bracket_start = content.find('[')?;
        let key = if bracket_start > 0 {
            let raw = content[..bracket_start].trim();
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

        let mut fields = None;
        let after_bracket = &content[bracket_end + 1..];
        if after_bracket.starts_with('{') {
            if let Some(brace_end) = after_bracket.find('}') {
                let fields_str = &after_bracket[1..brace_end];
                let parts = self.helper.split_top_level(fields_str, delimiter);
                fields = Some(parts.into_iter().map(|s| s.trim()).collect());
            }
        }

        let colon_pos = content.rfind(':')?;
        let after_colon = content[colon_pos + 1..].trim();
        let inline_values = if !after_colon.is_empty() { Some(after_colon) } else { None };

        Some(ArrayHeader { key, length, delimiter, fields, inline_values })
    }

    fn parse_key<'a>(&mut self, content: &'a str) -> Result<(&'a str, usize), String> {
        if content.starts_with('"') {
            let close = self.helper.find_closing_quote(content, 0)
                .ok_or_else(|| "Unterminated quoted key".to_string())?;
            let key = &content[1..close];
            if close + 1 >= content.len() || content.as_bytes()[close + 1] != b':' {
                return Err("Missing colon after key".to_string());
            }
            Ok((key, close + 2))
        } else {
            let colon = content.find(':').ok_or_else(|| "Missing colon after key".to_string())?;
            Ok((content[..colon].trim(), colon + 1))
        }
    }

    fn emit_inline_value<'a>(&mut self, input: &'a str, tape: &mut Vec<Node<'a>>) {
        let trimmed = input.trim();
        if trimmed.is_empty() { tape.push(Node::String("")); return; }
        if trimmed == "[]" { tape.push(Node::Array(0)); return; }
        if trimmed == "{}" { tape.push(Node::Object(0)); return; }

        if trimmed.starts_with('{') {
            if let Some(inner) = self.helper.extract_brace_content(trimmed) {
                self.emit_inline_object(inner, tape);
                return;
            }
        }

        if trimmed.starts_with('[') {
            if let Some(close) = self.helper.find_matching_bracket(trimmed, 0) {
                let inner = &trimmed[1..close];
                let arr_pos = tape.len();
                tape.push(Node::Array(0));
                let count = self.emit_delimited_values(inner, ',', tape);
                tape[arr_pos] = Node::Array(count);
                return;
            }
        }

        self.emit_primitive(trimmed, tape);
    }

    fn emit_inline_object<'a>(&mut self, inner: &'a str, tape: &mut Vec<Node<'a>>) {
        let obj_pos = tape.len();
        tape.push(Node::Object(0));
        let mut count = 0;
        let parts = self.helper.split_top_level(inner, ',');

        for part in &parts {
            let part = part.trim();
            if part.is_empty() { continue; }
            if let Some(colon) = self.helper.find_key_colon(part) {
                let key = part[..colon].trim();
                let key = if key.starts_with('"') && key.ends_with('"') && key.len() >= 2 {
                    &key[1..key.len() - 1]
                } else {
                    key
                };
                tape.push(Node::Key(key));
                self.emit_inline_value(part[colon + 1..].trim(), tape);
                count += 1;
            }
        }

        tape[obj_pos] = Node::Object(count);
    }

    fn emit_delimited_values<'a>(&mut self, input: &'a str, delimiter: char, tape: &mut Vec<Node<'a>>) -> usize {
        let mut count = 0;
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
                self.emit_primitive(input[seg_start..i].trim(), tape);
                count += 1;
                seg_start = i + 1;
                i += 1;
                continue;
            }
            i += 1;
        }
        if seg_start < input.len() || count > 0 {
            self.emit_primitive(input[seg_start..].trim(), tape);
            count += 1;
        }
        count
    }

    fn emit_primitive<'a>(&mut self, token: &'a str, tape: &mut Vec<Node<'a>>) {
        let t = token.trim();
        if t.is_empty() { tape.push(Node::String("")); return; }
        if t == "[]" { tape.push(Node::Array(0)); return; }
        if t == "{}" { tape.push(Node::Object(0)); return; }

        // Schema reference resolution
        if t.starts_with("@s:") || t.starts_with("@r:") || t.starts_with("@sec:") {
            let resolved = self.helper.resolve_reference(t);
            self.emit_resolved_value(&resolved, tape);
            return;
        }

        if t.starts_with('"') {
            if let Some(close) = self.helper.find_closing_quote(t, 0) {
                tape.push(Node::String(&t[1..close]));
                return;
            }
        }

        if t == "true" { tape.push(Node::Bool(true)); return; }
        if t == "false" { tape.push(Node::Bool(false)); return; }
        if t == "null" { tape.push(Node::Null); return; }

        let first = t.as_bytes()[0];
        if first.is_ascii_digit() || first == b'+' || first == b'-' || first == b'.' {
            if let Some(node) = self.try_parse_number(t) {
                tape.push(node);
                return;
            }
        }

        tape.push(Node::String(t));
    }

    fn try_parse_number(&self, t: &str) -> Option<Node<'static>> {
        let bytes = t.as_bytes();
        let len = bytes.len();
        let mut pos = 0;
        let neg = bytes[0] == b'-';
        if neg || bytes[0] == b'+' { pos += 1; }
        if pos >= len { return None; }

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

            if pos == len && !overflow {
                let val = if neg {
                    if n > (i64::MAX as u64) + 1 {
                        return t.parse::<f64>().ok().map(Node::Float);
                    }
                    if n == (i64::MAX as u64) + 1 { i64::MIN }
                    else { -(n as i64) }
                } else {
                    if n > i64::MAX as u64 {
                        return t.parse::<f64>().ok().map(Node::Float);
                    }
                    n as i64
                };
                return Some(Node::Integer(val));
            }

            if pos < len && (bytes[pos] == b'.' || bytes[pos] == b'e' || bytes[pos] == b'E') {
                return t.parse::<f64>().ok().map(Node::Float);
            }

            if overflow {
                return t.parse::<f64>().ok().map(Node::Float);
            }

            return None;
        }

        if bytes[pos] == b'.' {
            return t.parse::<f64>().ok().map(Node::Float);
        }

        None
    }

    // Pre-scan input for schema definitions (s:/r:/sec:) and register in helper's registry
    fn prescan_schema_defs(&mut self, input: &str) {
        for line in input.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            let first = trimmed.as_bytes()[0];
            if first == b's' && trimmed.starts_with("s:") {
                self.helper.parse_schema_def(trimmed, "s");
            } else if first == b'r' && trimmed.starts_with("r:") {
                self.helper.parse_schema_def(trimmed, "r");
            } else if trimmed.starts_with("sec:") {
                self.helper.parse_schema_def(trimmed, "sec");
            }
        }
    }

    // Store a string from resolved schema and return &'a str pointing to stable heap buffer.
    // SAFETY: String heap data doesn't move on Vec realloc; ownership transfers to Tape._resolved.
    fn alloc_resolved<'a>(&mut self, s: &str) -> &'a str {
        self.resolved_strs.push(s.to_string());
        let stored = self.resolved_strs.last().unwrap();
        unsafe { &*(stored.as_str() as *const str) }
    }

    // Emit an owned Value (from schema resolution) into the tape
    fn emit_resolved_value<'a>(&mut self, val: &crate::Value, tape: &mut Vec<Node<'a>>) {
        match val {
            crate::Value::Null => tape.push(Node::Null),
            crate::Value::Bool(b) => tape.push(Node::Bool(*b)),
            crate::Value::Integer(i) => tape.push(Node::Integer(*i)),
            crate::Value::Float(f) => tape.push(Node::Float(*f)),
            crate::Value::String(s) => {
                let borrowed: &'a str = self.alloc_resolved(s);
                tape.push(Node::String(borrowed));
            }
            crate::Value::Array(arr) => {
                let pos = tape.len();
                tape.push(Node::Array(arr.len()));
                for v in arr { self.emit_resolved_value(v, tape); }
                tape[pos] = Node::Array(arr.len());
            }
            crate::Value::Object(obj) => {
                let pos = tape.len();
                tape.push(Node::Object(obj.len()));
                for (k, v) in obj {
                    let key: &'a str = self.alloc_resolved(k);
                    tape.push(Node::Key(key));
                    self.emit_resolved_value(v, tape);
                }
                tape[pos] = Node::Object(obj.len());
            }
        }
    }

    // Fallback: convert owned Value to tape (for minified input)
    fn owned_to_tape(&self, val: &crate::Value) -> Tape<'static> {
        let mut nodes = Vec::new();
        self.emit_owned(val, &mut nodes);
        Tape { nodes, _resolved: Vec::new() }
    }

    fn emit_owned(&self, val: &crate::Value, tape: &mut Vec<Node<'static>>) {
        match val {
            crate::Value::Null => tape.push(Node::Null),
            crate::Value::Bool(b) => tape.push(Node::Bool(*b)),
            crate::Value::Integer(i) => tape.push(Node::Integer(*i)),
            crate::Value::Float(f) => tape.push(Node::Float(*f)),
            crate::Value::String(_) => tape.push(Node::String("")),
            crate::Value::Array(arr) => {
                let pos = tape.len();
                tape.push(Node::Array(arr.len()));
                for v in arr { self.emit_owned(v, tape); }
                tape[pos] = Node::Array(arr.len());
            }
            crate::Value::Object(obj) => {
                let pos = tape.len();
                tape.push(Node::Object(obj.len()));
                for (_, v) in obj {
                    tape.push(Node::Key(""));
                    self.emit_owned(v, tape);
                }
                tape[pos] = Node::Object(obj.len());
            }
        }
    }
}

impl Default for TapeDecoder {
    fn default() -> Self {
        Self::new()
    }
}
