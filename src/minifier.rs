// src/minifier.rs
// SCON Minifier — bidirectional transform between indented and single-line formats
//
// Minified SCON uses semicolons to encode structure that indentation provides in expanded form:
//   ';'   = newline at same or deeper depth (regular separator)
//   ';;'  = dedent by 1 level (closing a scope)
//   ';;;' = dedent by 2 levels, ';;;;' = 3, etc. (N semicolons = dedent N-1)
//
// Example:
//   Expanded:          Minified:
//   server:            server:;host: localhost;port: 8080;;db:;name: main
//     host: localhost
//     port: 8080
//   db:
//     name: main
//
// Both operations are O(L) streaming single-pass, O(L) space.
// Minify: tracks depth via indentation, emits ';' × (depth_diff + 1) on dedent.
// Expand: tracks depth via scope openers (trailing ':') and semicolon counts.
//
// The decoder can parse minified input directly (decode_minified) without expanding first,
// avoiding the intermediate String allocation.

pub struct Minifier;

impl Minifier {
    // Minify SCON to single line
    // ; = newline, ;; = dedent 1, ;;; = dedent 2, etc.
    pub fn minify(scon: &str) -> String {
        let mut result = String::with_capacity(scon.len());
        let mut prev_depth: isize = 0;
        let mut is_first = true;

        // Auto-detect indent
        let indent = Self::detect_indent(scon);

        for line in scon.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                if trimmed.starts_with("#!scon/") {
                    result.push_str(trimmed);
                    result.push(';');
                }
                continue;
            }

            let depth = Self::calculate_depth(line, indent) as isize;

            if !is_first {
                let diff = prev_depth - depth;
                if diff >= 2 {
                    for _ in 0..=(diff as usize) {
                        result.push(';');
                    }
                } else if diff == 1 {
                    result.push_str(";;");
                } else {
                    result.push(';');
                }
            }

            result.push_str(trimmed);
            prev_depth = depth;

            // Scope openers
            if trimmed.ends_with(':') {
                prev_depth = depth + 1;
            }
            // List items
            if trimmed.starts_with("- ") {
                prev_depth = depth + 1;
            }

            is_first = false;
        }

        result
    }

    // P3.2: Expand minified SCON — write directly to String without Vec<String> + join
    pub fn expand(minified: &str, indent: usize) -> String {
        let mut result = String::with_capacity(minified.len() * 2);
        let mut depth: usize = 0;
        let mut seg_start = 0;
        let mut in_quotes = false;
        let bytes = minified.as_bytes();
        let len = bytes.len();
        let mut is_first = true;

        // Pre-computed indent string (reusable)
        const SPACES: &str = "                                                                ";

        let mut i = 0;
        while i < len {
            let c = bytes[i];

            // Handle escape in quotes
            if c == b'\\' && in_quotes && i + 1 < len {
                i += 2;
                continue;
            }

            if c == b'"' {
                in_quotes = !in_quotes;
                i += 1;
                continue;
            }

            if c == b';' && !in_quotes {
                // Count consecutive semicolons
                let mut semi_count = 1usize;
                while i + 1 < len && bytes[i + 1] == b';' {
                    semi_count += 1;
                    i += 1;
                }

                // Emit current segment
                let segment = minified[seg_start..i - semi_count + 1].trim();
                if !segment.is_empty() {
                    if !is_first {
                        result.push('\n');
                    }
                    is_first = false;
                    // Write indent
                    let spaces = indent * depth;
                    if spaces > 0 {
                        if spaces <= SPACES.len() {
                            result.push_str(&SPACES[..spaces]);
                        } else {
                            for _ in 0..spaces { result.push(' '); }
                        }
                    }
                    result.push_str(segment);

                    // Scope openers
                    if segment.ends_with(':') && !Self::has_value_after_colon(segment) {
                        depth += 1;
                    }
                    // List items
                    if segment.starts_with("- ") {
                        depth += 1;
                    }
                }

                // Apply dedent
                if semi_count >= 2 {
                    depth = depth.saturating_sub(semi_count - 1);
                }

                seg_start = i + 1;
                i += 1;
                continue;
            }

            i += 1;
        }

        // Last segment
        let segment = minified[seg_start..].trim();
        if !segment.is_empty() {
            if !is_first {
                result.push('\n');
            }
            let spaces = indent * depth;
            if spaces > 0 {
                if spaces <= SPACES.len() {
                    result.push_str(&SPACES[..spaces]);
                } else {
                    for _ in 0..spaces { result.push(' '); }
                }
            }
            result.push_str(segment);
        }

        result
    }

    fn has_value_after_colon(s: &str) -> bool {
        if let Some(colon_pos) = s.rfind(':') {
            let after = s[colon_pos + 1..].trim();
            !after.is_empty()
        } else {
            false
        }
    }

    fn detect_indent(scon: &str) -> usize {
        for line in scon.lines() {
            let spaces = line.len() - line.trim_start_matches(' ').len();
            if spaces > 0 && !line.trim().is_empty() && !line.trim().starts_with('#') {
                return spaces;
            }
        }
        1
    }

    fn calculate_depth(line: &str, indent: usize) -> usize {
        let spaces = line.len() - line.trim_start_matches(' ').len();
        if indent > 0 { spaces / indent } else { 0 }
    }
}
