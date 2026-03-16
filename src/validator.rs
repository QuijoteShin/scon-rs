// src/validator.rs
// SCON data validator with three strictness modes.
//
// Modes control how issues are reported:
//   loose: silently ignore all issues
//   warn: collect warnings (default in PHP/JS)
//   strict: collect errors (blocks processing)
//
// enforce rules validate against known specs (openapi:3.1, openapi:3.0).
// Required fields use '!' suffix convention from PHP: "field!" means required.

use crate::value::Value;
use compact_str::CompactString;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValidationMode {
    Loose,
    Warn,
    Strict,
}

#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub valid: bool,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub mode: ValidationMode,
    pub enforce: Option<String>,
}

pub struct Validator {
    mode: ValidationMode,
    enforce: Option<String>,
}

impl Validator {
    pub fn new(mode: ValidationMode) -> Self {
        Self { mode, enforce: None }
    }

    pub fn with_enforce(mut self, spec: &str) -> Self {
        self.enforce = Some(spec.to_string());
        self
    }

    pub fn validate(&self, data: &Value) -> ValidationResult {
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        if !matches!(data, Value::Object(_) | Value::Array(_)) {
            add_issue(&self.mode, &self.enforce, "Root value must be an object or array", &mut warnings, &mut errors);
        }

        if let Some(ref spec) = self.enforce {
            if let Value::Object(obj) = data {
                self.enforce_spec(obj, spec, &mut warnings, &mut errors);
            }
        }

        ValidationResult {
            valid: errors.is_empty(),
            warnings,
            errors,
            mode: self.mode,
            enforce: self.enforce.clone(),
        }
    }

    pub fn validate_schema(&self, name: &str, schema: &Value) -> ValidationResult {
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        if let Value::Object(obj) = schema {
            if obj.is_empty() {
                add_issue(&self.mode, &self.enforce, &format!("Schema '{}' is empty", name), &mut warnings, &mut errors);
            }
        }

        ValidationResult {
            valid: errors.is_empty(),
            warnings,
            errors,
            mode: self.mode,
            enforce: self.enforce.clone(),
        }
    }

    pub fn validate_against_schema(
        &self, data: &Value, schema: &Value, path: &str
    ) -> ValidationResult {
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        if let (Value::Object(data_obj), Value::Object(schema_obj)) = (data, schema) {
            // Check required fields
            for (key, _) in schema_obj {
                let key_str = key.as_str();
                if key_str.ends_with('!') {
                    let clean_key = &key_str[..key_str.len() - 1];
                    let field_path = if path.is_empty() {
                        clean_key.to_string()
                    } else {
                        format!("{}.{}", path, clean_key)
                    };
                    if !data_obj.contains_key(clean_key) {
                        add_issue(&self.mode, &self.enforce,
                            &format!("Missing required field: {}", field_path),
                            &mut warnings, &mut errors);
                    }
                }
            }

            // Check for extra fields
            let schema_keys: Vec<String> = schema_obj.keys()
                .map(|k| k.trim_end_matches('!').to_string())
                .collect();

            match self.mode {
                ValidationMode::Strict => {
                    for key in data_obj.keys() {
                        if !schema_keys.iter().any(|sk| sk == key.as_str()) {
                            let field_path = if path.is_empty() {
                                key.to_string()
                            } else {
                                format!("{}.{}", path, key)
                            };
                            errors.push(format!("Extra field not in schema: {}", field_path));
                        }
                    }
                }
                ValidationMode::Warn => {
                    for key in data_obj.keys() {
                        if !schema_keys.iter().any(|sk| sk == key.as_str()) {
                            let field_path = if path.is_empty() {
                                key.to_string()
                            } else {
                                format!("{}.{}", path, key)
                            };
                            warnings.push(format!("Extra field: {}", field_path));
                        }
                    }
                }
                ValidationMode::Loose => {}
            }
        }

        ValidationResult {
            valid: errors.is_empty(),
            warnings,
            errors,
            mode: self.mode,
            enforce: self.enforce.clone(),
        }
    }

    fn enforce_spec(
        &self, obj: &crate::value::SconMap<CompactString, Value>,
        spec: &str, warnings: &mut Vec<String>, errors: &mut Vec<String>
    ) {
        let rules = match spec {
            "openapi:3.1" | "openapi:3.0" => &OPENAPI_RULES,
            _ => return,
        };

        for rule in rules {
            match rule {
                EnforceRule::Required(fields) => {
                    for field in *fields {
                        if !obj.contains_key(*field) {
                            add_issue(&self.mode, &self.enforce,
                                &format!("Missing required field per {}: {}", spec, field),
                                warnings, errors);
                        }
                    }
                }
                EnforceRule::NestedRequired(parent, fields) => {
                    if let Some(Value::Object(parent_obj)) = obj.get(*parent) {
                        for field in *fields {
                            if !parent_obj.contains_key(*field) {
                                add_issue(&self.mode, &self.enforce,
                                    &format!("Missing required field per {}: {}.{}", spec, parent, field),
                                    warnings, errors);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn add_issue(
    mode: &ValidationMode, enforce: &Option<String>,
    msg: &str, warnings: &mut Vec<String>, errors: &mut Vec<String>
) {
    if *mode == ValidationMode::Strict || enforce.is_some() {
        errors.push(msg.to_string());
    } else if *mode == ValidationMode::Warn {
        warnings.push(msg.to_string());
    }
    // Loose: ignore
}

enum EnforceRule {
    Required(&'static [&'static str]),
    NestedRequired(&'static str, &'static [&'static str]),
}

static OPENAPI_RULES: [EnforceRule; 2] = [
    EnforceRule::Required(&["openapi", "info", "paths"]),
    EnforceRule::NestedRequired("info", &["title", "version"]),
];
