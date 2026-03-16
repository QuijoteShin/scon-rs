// src/borrowed.rs
// Zero-copy SCON decoder — returns BorrowedValue<'a> with &'a str borrowed from input.
//
// Strings without escapes (~90%): &'a str directly from input buffer — zero allocation.
// Strings with escapes (~10%): unescaped into bumpalo arena — &'a str from arena.
// Keys: &'a str in all cases (unquoted keys borrow directly, quoted keys from arena).
//
// This eliminates CompactString::from() copies that account for ~15-25% of decode time.
// Trade-off: returned BorrowedValue borrows from both input and arena — both must outlive it.

use crate::decoder::Decoder;
use bumpalo::Bump;
use indexmap::IndexMap;
use memchr::memchr;

pub type BorrowedMap<'a> = IndexMap<&'a str, BorrowedValue<'a>, ahash::RandomState>;

#[derive(Debug, Clone, PartialEq)]
pub enum BorrowedValue<'a> {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(&'a str),
    Array(Vec<BorrowedValue<'a>>),
    Object(BorrowedMap<'a>),
}

impl<'a> BorrowedValue<'a> {
    #[inline]
    pub fn is_primitive(&self) -> bool {
        matches!(self, BorrowedValue::Null | BorrowedValue::Bool(_) | BorrowedValue::Integer(_) | BorrowedValue::Float(_) | BorrowedValue::String(_))
    }
}

// Reuse ParsedLine and ArrayHeader from decoder (same layout, same lifetime)
struct ParsedLine<'a> {
    depth: usize,
    content: &'a str,
    _line_num: usize,
    has_bracket: bool,
}

struct ArrayHeader<'a> {
    key: Option<&'a str>,
    length: usize,
    delimiter: char,
    fields: Option<Vec<&'a str>>,
    inline_values: Option<&'a str>,
}

pub struct BorrowedDecoder<'alloc> {
    helper: Decoder, // reuse string-level helpers (find_key_colon, find_closing_quote, etc.)
    alloc: &'alloc Bump,
    scratch: String,
    indent: usize,
    indent_auto_detect: bool,
}

impl<'alloc> BorrowedDecoder<'alloc> {
    pub fn new(alloc: &'alloc Bump) -> Self {
        Self {
            helper: Decoder::new(),
            alloc,
            scratch: String::with_capacity(256),
            indent: 1,
            indent_auto_detect: true,
        }
    }

    pub fn decode<'a>(&mut self, input: &'a str) -> Result<BorrowedValue<'a>, String>
    where 'alloc: 'a
    {
        if !input.contains('\n') && input.contains(';') {
            // Minified — delegate to owned decoder for now (not the hot path we're optimizing)
            let val = Decoder::new().decode(input)?;
            return Ok(self.owned_to_borrowed(&val));
        }

        // Auto-detect indent
        if self.indent_auto_detect {
            if let Some(cap) = input.find('\n').and_then(|nl_pos| {
                let after = &input[nl_pos + 1..];
                let spaces = after.len() - after.trim_start_matches(' ').len();
                if spaces > 0 && after.len() > spaces && !after.as_bytes().get(spaces).map_or(true, |b| b.is_ascii_whitespace()) {
                    Some(spaces)
                } else {
                    None
                }
            }) {
                self.indent = cap;
            } else {
                for line in input.lines() {
                    let spaces = line.len() - line.trim_start_matches(' ').len();
                    if spaces > 0 && !line.trim().is_empty() && !line.trim().starts_with('#') {
                        self.indent = spaces;
                        break;
                    }
                }
            }
        }

        // Pre-scan schema definitions before main parse
        self.prescan_schema_defs(input);

        let line_estimate = input.as_bytes().iter().filter(|&&b| b == b'\n').count() + 1;
        let mut parsed: Vec<ParsedLine<'_>> = Vec::with_capacity(line_estimate);
        let indent = self.indent;
        for (line_num, line) in input.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            let first_byte = trimmed.as_bytes()[0];
            if first_byte == b'#' { continue; }
            if first_byte == b'@' && (trimmed.starts_with("@@") || trimmed.starts_with("@use ")) { continue; }
            if first_byte == b's' && trimmed.starts_with("s:") { continue; }
            if first_byte == b'r' && trimmed.starts_with("r:") { continue; }
            if trimmed.starts_with("sec:") { continue; }
            let spaces = line.len() - line.trim_start_matches(' ').len();
            let depth = if indent > 0 { spaces / indent } else { 0 };
            let has_bracket = memchr(b'[', trimmed.as_bytes())
                .map_or(false, |bp| memchr(b':', trimmed.as_bytes()).map_or(false, |cp| bp < cp));
            parsed.push(ParsedLine { depth, content: trimmed, _line_num: line_num, has_bracket });
        }

        if parsed.is_empty() {
            return Ok(BorrowedValue::Object(BorrowedMap::default()));
        }

        let first = &parsed[0];
        if parsed.len() == 1 && first.content == "{}" {
            return Ok(BorrowedValue::Object(BorrowedMap::default()));
        }

        if let Some(header) = self.try_array_header(first.content) {
            if header.key.is_none() {
                return self.decode_array_from_header(0, &parsed, &header).map(|(v, _)| v);
            }
        }

        if parsed.len() == 1 && !first.content.contains(':') {
            return Ok(self.parse_primitive(first.content));
        }

        self.decode_object(0, &parsed, 0).map(|(obj, _)| BorrowedValue::Object(obj))
    }

    // --- Object decoding ---

    fn decode_object<'a>(&mut self, base_depth: usize, lines: &[ParsedLine<'a>], start: usize) -> Result<(BorrowedMap<'a>, usize), String>
    where 'alloc: 'a
    {
        let mut result = BorrowedMap::default();
        let mut i = start;

        while i < lines.len() {
            let line = &lines[i];
            if line.depth < base_depth { break; }
            if line.depth > base_depth { i += 1; continue; }

            let content = line.content;

            if line.has_bracket {
                if let Some(header) = self.try_array_header(content) {
                    if let Some(key) = header.key {
                        let (val, next_i) = self.decode_array_from_header(i, lines, &header)?;
                        result.insert(key, val);
                        i = next_i;
                        continue;
                    }
                }
            }

            if let Some(colon_pos) = self.helper.find_key_colon(content) {
                let (key, value, next_i) = self.decode_key_value(line, lines, i, base_depth, colon_pos)?;
                result.insert(key, value);
                i = next_i;
                continue;
            }

            i += 1;
        }

        Ok((result, i))
    }

    fn decode_key_value<'a>(&mut self, line: &ParsedLine<'a>, lines: &[ParsedLine<'a>], index: usize, base_depth: usize, colon_pos: usize) -> Result<(&'a str, BorrowedValue<'a>, usize), String>
    where 'alloc: 'a
    {
        let content = line.content;
        let (key, key_end) = if content.as_bytes()[0] == b'"' {
            self.parse_key(content)?
        } else {
            let k = content[..colon_pos].trim();
            (k, colon_pos + 1)
        };
        let rest = content[key_end..].trim();

        if !rest.is_empty() {
            let value = self.parse_inline_value(rest);
            return Ok((key, value, index + 1));
        }

        if index + 1 < lines.len() && lines[index + 1].depth > base_depth {
            let child_depth = base_depth + 1;
            if index + 1 < lines.len() && lines[index + 1].depth == child_depth && lines[index + 1].content.starts_with("- ") {
                let mut items = Vec::new();
                let mut j = index + 1;
                while j < lines.len() && lines[j].depth >= child_depth {
                    if lines[j].depth == child_depth && lines[j].content.starts_with("- ") {
                        let item_content = &lines[j].content[2..];
                        if item_content.contains(':') {
                            let (obj, next_j) = self.decode_list_item_object(&lines[j], lines, j, base_depth)?;
                            items.push(BorrowedValue::Object(obj));
                            j = next_j;
                            continue;
                        } else {
                            items.push(self.parse_inline_value(item_content));
                        }
                    }
                    j += 1;
                }
                return Ok((key, BorrowedValue::Array(items), j));
            }

            let (obj, next_i) = self.decode_object(base_depth + 1, lines, index + 1)?;
            return Ok((key, BorrowedValue::Object(obj), next_i));
        }

        Ok((key, BorrowedValue::Object(BorrowedMap::default()), index + 1))
    }

    // --- Array decoding ---

    fn decode_array_from_header<'a>(&mut self, index: usize, lines: &[ParsedLine<'a>], header: &ArrayHeader<'a>) -> Result<(BorrowedValue<'a>, usize), String>
    where 'alloc: 'a
    {
        if header.length == 0 {
            return Ok((BorrowedValue::Array(vec![]), index + 1));
        }

        if let Some(ref inline) = header.inline_values {
            if header.fields.is_none() {
                let values = self.parse_delimited_values(inline, header.delimiter);
                return Ok((BorrowedValue::Array(values), index + 1));
            }
        }

        if let Some(ref fields) = header.fields {
            return self.decode_tabular_array(index, lines, header.length, fields, header.delimiter);
        }

        self.decode_expanded_array(index, lines, header.length)
    }

    fn decode_tabular_array<'a>(&mut self, header_idx: usize, lines: &[ParsedLine<'a>], expected: usize, fields: &[&'a str], delimiter: char) -> Result<(BorrowedValue<'a>, usize), String>
    where 'alloc: 'a
    {
        let base_depth = lines[header_idx].depth;
        let mut result = Vec::with_capacity(expected);
        let mut i = header_idx + 1;

        while i < lines.len() && result.len() < expected {
            if lines[i].depth != base_depth + 1 { break; }
            let mut values = self.parse_delimited_values(lines[i].content, delimiter);
            values.resize(fields.len(), BorrowedValue::Null);
            let mut row = BorrowedMap::with_capacity_and_hasher(fields.len(), ahash::RandomState::new());
            for (field, val) in fields.iter().zip(values.drain(..)) {
                row.insert(*field, val);
            }
            result.push(BorrowedValue::Object(row));
            i += 1;
        }

        Ok((BorrowedValue::Array(result), i))
    }

    fn decode_expanded_array<'a>(&mut self, header_idx: usize, lines: &[ParsedLine<'a>], expected: usize) -> Result<(BorrowedValue<'a>, usize), String>
    where 'alloc: 'a
    {
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
                        result.push(BorrowedValue::Object(obj));
                        i = next_i;
                        continue;
                    } else if let Some(ref inline) = header.inline_values {
                        result.push(BorrowedValue::Array(self.parse_delimited_values(inline, header.delimiter)));
                    }
                } else if item_content.contains(':') {
                    let (obj, next_i) = self.decode_list_item_object(line, lines, i, base_depth)?;
                    result.push(BorrowedValue::Object(obj));
                    i = next_i;
                    continue;
                } else {
                    result.push(self.parse_inline_value(item_content));
                }
            }
            i += 1;
        }

        Ok((BorrowedValue::Array(result), i))
    }

    fn decode_list_item_object<'a>(&mut self, _line: &ParsedLine<'a>, lines: &[ParsedLine<'a>], index: usize, base_depth: usize) -> Result<(BorrowedMap<'a>, usize), String>
    where 'alloc: 'a
    {
        let item_content = &lines[index].content[2..];
        let mut result = BorrowedMap::default();
        let cont_depth = base_depth + 2;
        let mut cont_start = index + 1;

        if let Some(header) = self.try_array_header(item_content) {
            if let Some(key) = header.key {
                let (val, next_i) = self.decode_array_from_header(index, lines, &header)?;
                result.insert(key, val);
                cont_start = next_i;
            }
        } else {
            let (key, key_end) = self.parse_key(item_content)?;
            let rest = item_content[key_end..].trim();

            if !rest.is_empty() {
                result.insert(key, self.parse_inline_value(rest));
            } else if index + 1 < lines.len() && lines[index + 1].depth >= cont_depth {
                let (obj, next_i) = self.decode_object(cont_depth, lines, index + 1)?;
                result.insert(key, BorrowedValue::Object(obj));
                cont_start = next_i;
            } else {
                result.insert(key, BorrowedValue::Object(BorrowedMap::default()));
            }
        }

        let mut i = cont_start;
        while i < lines.len() {
            let next = &lines[i];
            if next.depth < cont_depth { break; }
            if next.depth == cont_depth {
                if next.content.starts_with("- ") { break; }

                if next.has_bracket {
                    if let Some(header) = self.try_array_header(next.content) {
                        if let Some(k) = header.key {
                            let (val, next_i) = self.decode_array_from_header(i, lines, &header)?;
                            result.insert(k, val);
                            i = next_i;
                            continue;
                        }
                    }
                }

                if let Some(colon_pos) = self.helper.find_key_colon(next.content) {
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

    // --- Schema pre-scan and ref resolution ---

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

    // --- Parsing helpers (delegate string ops to inner Decoder, build BorrowedValue) ---

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

    fn parse_key<'a>(&mut self, content: &'a str) -> Result<(&'a str, usize), String>
    where 'alloc: 'a
    {
        if content.starts_with('"') {
            let close = self.helper.find_closing_quote(content, 0)
                .ok_or_else(|| "Unterminated quoted key".to_string())?;
            let raw = &content[1..close];
            let key = self.unescape_borrowed(raw);
            if close + 1 >= content.len() || content.as_bytes()[close + 1] != b':' {
                return Err("Missing colon after key".to_string());
            }
            Ok((key, close + 2))
        } else {
            let colon = content.find(':').ok_or_else(|| "Missing colon after key".to_string())?;
            Ok((content[..colon].trim(), colon + 1))
        }
    }

    fn unquote_key<'a>(&mut self, s: &'a str) -> &'a str
    where 'alloc: 'a
    {
        let t = s.trim();
        if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
            self.unescape_borrowed(&t[1..t.len() - 1])
        } else {
            t
        }
    }

    fn parse_inline_value<'a>(&mut self, input: &'a str) -> BorrowedValue<'a>
    where 'alloc: 'a
    {
        let trimmed = input.trim();
        if trimmed.is_empty() { return BorrowedValue::String(""); }
        if trimmed == "[]" { return BorrowedValue::Array(vec![]); }
        if trimmed == "{}" { return BorrowedValue::Object(BorrowedMap::default()); }

        if trimmed.starts_with('{') {
            if let Some(inner) = self.helper.extract_brace_content(trimmed) {
                return BorrowedValue::Object(self.parse_inline_object(inner));
            }
        }

        if trimmed.starts_with('[') {
            if let Some(close) = self.helper.find_matching_bracket(trimmed, 0) {
                let inner = &trimmed[1..close];
                let items = self.parse_delimited_values(inner, ',');
                return BorrowedValue::Array(items);
            }
        }

        self.parse_primitive(trimmed)
    }

    fn parse_inline_object<'a>(&mut self, inner: &'a str) -> BorrowedMap<'a>
    where 'alloc: 'a
    {
        let mut result = BorrowedMap::default();
        let parts = self.helper.split_top_level(inner, ',');

        for part in &parts {
            let part = part.trim();
            if part.is_empty() { continue; }
            if let Some(colon) = self.helper.find_key_colon(part) {
                let key = self.unquote_key(part[..colon].trim());
                let val = self.parse_inline_value(part[colon + 1..].trim());
                result.insert(key, val);
            }
        }

        result
    }

    fn parse_delimited_values<'a>(&mut self, input: &'a str, delimiter: char) -> Vec<BorrowedValue<'a>>
    where 'alloc: 'a
    {
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

    fn parse_primitive<'a>(&mut self, token: &'a str) -> BorrowedValue<'a>
    where 'alloc: 'a
    {
        let t = token.trim();
        if t.is_empty() { return BorrowedValue::String(""); }
        if t == "[]" { return BorrowedValue::Array(vec![]); }
        if t == "{}" { return BorrowedValue::Object(BorrowedMap::default()); }

        // Schema reference resolution
        if t.starts_with("@s:") || t.starts_with("@r:") || t.starts_with("@sec:") {
            let resolved = self.helper.resolve_reference(t);
            return self.owned_to_borrowed(&resolved);
        }

        if t.starts_with('"') {
            if let Some(close) = self.helper.find_closing_quote(t, 0) {
                let raw = &t[1..close];
                return BorrowedValue::String(self.unescape_borrowed(raw));
            }
        }

        if t == "true" { return BorrowedValue::Bool(true); }
        if t == "false" { return BorrowedValue::Bool(false); }
        if t == "null" { return BorrowedValue::Null; }

        let first = t.as_bytes()[0];
        if first.is_ascii_digit() || first == b'+' || first == b'-' || first == b'.' {
            if let Some(val) = self.try_parse_number(t) {
                return val;
            }
        }

        // Unquoted string — borrow directly from input, zero copy
        BorrowedValue::String(t)
    }

    fn try_parse_number(&self, t: &str) -> Option<BorrowedValue<'static>> {
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
                        return t.parse::<f64>().ok().map(BorrowedValue::Float);
                    }
                    if n == (i64::MAX as u64) + 1 { i64::MIN }
                    else { -(n as i64) }
                } else {
                    if n > i64::MAX as u64 {
                        return t.parse::<f64>().ok().map(BorrowedValue::Float);
                    }
                    n as i64
                };
                return Some(BorrowedValue::Integer(val));
            }

            if pos < len && (bytes[pos] == b'.' || bytes[pos] == b'e' || bytes[pos] == b'E') {
                return t.parse::<f64>().ok().map(BorrowedValue::Float);
            }

            if overflow {
                return t.parse::<f64>().ok().map(BorrowedValue::Float);
            }

            return None;
        }

        if bytes[pos] == b'.' {
            return t.parse::<f64>().ok().map(BorrowedValue::Float);
        }

        None
    }

    // Zero-copy unescape: if no escapes, return &str from input; otherwise unescape into bump arena
    fn unescape_borrowed<'a>(&mut self, s: &'a str) -> &'a str
    where 'alloc: 'a
    {
        // Fast path: no backslashes → borrow directly from input
        if memchr(b'\\', s.as_bytes()).is_none() {
            return s;
        }
        // Slow path: unescape into scratch, then alloc_str into bump arena
        self.scratch.clear();
        let mut i = 0;
        while i < s.len() {
            match memchr(b'\\', &s.as_bytes()[i..]) {
                Some(offset) => {
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
                    self.scratch.push_str(&s[i..]);
                    break;
                }
            }
        }
        // Allocate in bump arena — lives as long as 'alloc
        self.alloc.alloc_str(&self.scratch)
    }

    // Fallback: convert owned Value to BorrowedValue (for minified path)
    fn owned_to_borrowed<'a>(&self, val: &crate::Value) -> BorrowedValue<'a>
    where 'alloc: 'a
    {
        match val {
            crate::Value::Null => BorrowedValue::Null,
            crate::Value::Bool(b) => BorrowedValue::Bool(*b),
            crate::Value::Integer(i) => BorrowedValue::Integer(*i),
            crate::Value::Float(f) => BorrowedValue::Float(*f),
            crate::Value::String(s) => BorrowedValue::String(self.alloc.alloc_str(s)),
            crate::Value::Array(arr) => {
                BorrowedValue::Array(arr.iter().map(|v| self.owned_to_borrowed(v)).collect())
            }
            crate::Value::Object(obj) => {
                let mut map = BorrowedMap::with_capacity_and_hasher(obj.len(), ahash::RandomState::new());
                for (k, v) in obj {
                    map.insert(self.alloc.alloc_str(k), self.owned_to_borrowed(v));
                }
                BorrowedValue::Object(map)
            }
        }
    }
}
