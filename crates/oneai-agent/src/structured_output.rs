//! Structured output validation + ModelRetry — JSON Schema validation of model
//! outputs with automatic re-prompting for self-correction.
//!
//! This is the "Rust 版 PydanticAI" pattern: when a model's final output doesn't
//! conform to the required JSON Schema, the AgentLoop injects the validation error
//! into the conversation and re-prompts the model for self-correction.
//!
//! Key concepts:
//! - **StructuredOutputConfig**: defines the JSON Schema, max retries, and re-prompt policy
//! - **ModelRetry**: the re-prompt context (error message, failed output, expected schema)
//! - **validate_json_schema()**: validates a string against a JSON Schema
//! - **build_retry_prompt()**: generates the re-prompt message for ModelRetry
//!
//! The validation happens at the DirectAnswer stage in the AgentLoop. If validation
//! fails and re_prompt_on_failure is true, the loop continues (without incrementing
//! iteration count) with the validation error injected as a system message.

use serde::{Deserialize, Serialize};

use oneai_core::StructuredOutputConfig;

// ─── ValidationResult ──────────────────────────────────────────────────────────

/// Result of validating a string output against a JSON Schema.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Whether the validation passed.
    pub passed: bool,

    /// Validation errors (if any).
    pub errors: Vec<ValidationError>,

    /// The parsed JSON output (if successfully parsed and validated).
    pub parsed_output: Option<serde_json::Value>,
}

impl ValidationResult {
    /// Create a passing validation result.
    pub fn passed(parsed: serde_json::Value) -> Self {
        Self {
            passed: true,
            errors: Vec::new(),
            parsed_output: Some(parsed),
        }
    }

    /// Create a failing validation result.
    pub fn failed(errors: Vec<ValidationError>) -> Self {
        Self {
            passed: false,
            errors,
            parsed_output: None,
        }
    }

    /// Get a human-readable summary of all validation errors.
    pub fn error_summary(&self) -> String {
        if self.errors.is_empty() {
            "No validation errors".to_string()
        } else {
            self.errors.iter()
                .map(|e| format!("At {}: expected {}, got {} — {}", e.path, e.expected, e.actual, e.message))
                .collect::<Vec<_>>()
                .join("; ")
        }
    }
}

// ─── ValidationError ────────────────────────────────────────────────────────────

/// A single JSON Schema validation error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    /// The JSON path where the error occurred (e.g., "/properties/name").
    pub path: String,

    /// What was expected at this path.
    pub expected: String,

    /// What was actually found.
    pub actual: String,

    /// The error message.
    pub message: String,
}

// ─── validate_json_schema ────────────────────────────────────────────────────────

/// Validate a string output against a JSON Schema.
///
/// This function:
/// 1. Attempts to parse the string as JSON
/// 2. If parsing succeeds, validates the JSON against the schema
/// 3. Returns a ValidationResult with any errors
///
/// For the AgentLoop's StructuredOutput integration, this is called
/// when the model produces a DirectAnswer. If the result is `failed`,
/// the AgentLoop may trigger a ModelRetry re-prompt.
pub fn validate_json_schema(output: &str, schema: &serde_json::Value) -> ValidationResult {
    // Step 1: Parse the output as JSON
    let parsed: serde_json::Value = match serde_json::from_str(output) {
        Ok(v) => v,
        Err(e) => {
            // The output isn't valid JSON at all
            return ValidationResult::failed(vec![ValidationError {
                path: "/".to_string(),
                expected: "valid JSON".to_string(),
                actual: format!("non-JSON text: {} chars", output.len()),
                message: format!("Output is not valid JSON: {}", e),
            }]);
        }
    };

    // Step 2: Validate against the schema
    // We use a simple schema validation approach:
    // - Check required properties
    // - Check property types
    // - Check string patterns/min/max
    // - Check array items
    //
    // For a production implementation, this would use the `jsonschema` crate
    // for full JSON Schema draft-07/2020-12 compliance. For now, we implement
    // a pragmatic subset that covers the most common validation scenarios:
    // type checking, required fields, and basic constraints.
    let errors = validate_schema_subset(&parsed, schema, "/");

    if errors.is_empty() {
        ValidationResult::passed(parsed)
    } else {
        ValidationResult::failed(errors)
    }
}

/// Validate a JSON value against a schema subset.
///
/// This implements a pragmatic subset of JSON Schema validation:
/// - `type`: check value type (string, number, integer, boolean, array, object, null)
/// - `required`: check required properties in objects
/// - `properties`: validate nested properties
/// - `enum`: check value is in the allowed list
/// - `minLength/maxLength`: check string length constraints
/// - `minimum/maximum`: check numeric range constraints
/// - `items`: validate array items against a sub-schema
/// - `additionalProperties`: check for unexpected properties
fn validate_schema_subset(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // Check type
    if let Some(schema_type) = schema.get("type").and_then(|t| t.as_str()) {
        let actual_type = json_type_of(value);
        if !type_matches(actual_type, schema_type) {
            errors.push(ValidationError {
                path: path.to_string(),
                expected: schema_type.to_string(),
                actual: actual_type.to_string(),
                message: format!("Expected type '{}' but got '{}'", schema_type, actual_type),
            });
            return errors; // Type mismatch → skip further validation
        }
    }

    // Object-specific validations
    if value.is_object() {
        let obj = value.as_object().unwrap();

        // Check required properties
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            for req in required {
                if let Some(req_name) = req.as_str() {
                    if !obj.contains_key(req_name) {
                        errors.push(ValidationError {
                            path: format!("{}.{}", path, req_name),
                            expected: "required property".to_string(),
                            actual: "missing".to_string(),
                            message: format!("Required property '{}' is missing", req_name),
                        });
                    }
                }
            }
        }

        // Validate properties
        if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
            for (prop_name, prop_schema) in properties {
                if let Some(prop_value) = obj.get(prop_name) {
                    let prop_path = format!("{}.{}", path, prop_name);
                    errors.extend(validate_schema_subset(prop_value, prop_schema, &prop_path));
                }
            }
        }

        // Check additionalProperties (if false, no extra properties allowed)
        if let Some(additional) = schema.get("additionalProperties") {
            if additional.is_boolean() && !additional.as_bool().unwrap() {
                let allowed_props: std::collections::HashSet<&str> = schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|o| o.keys().map(|k| k.as_str()).collect())
                    .unwrap_or_default();
                for key in obj.keys() {
                    if !allowed_props.contains(key.as_str()) {
                        errors.push(ValidationError {
                            path: format!("{}.{}", path, key),
                            expected: "no additional properties".to_string(),
                            actual: format!("unexpected property '{}'", key),
                            message: format!("Additional property '{}' is not allowed", key),
                        });
                    }
                }
            }
        }
    }

    // String-specific validations
    if value.is_string() {
        let s = value.as_str().unwrap();
        let len = s.len();

        if let Some(min_length) = schema.get("minLength").and_then(|v| v.as_u64()) {
            if len < min_length as usize {
                errors.push(ValidationError {
                    path: path.to_string(),
                    expected: format!("minLength {}", min_length),
                    actual: format!("length {}", len),
                    message: format!("String length {} is less than minLength {}", len, min_length),
                });
            }
        }

        if let Some(max_length) = schema.get("maxLength").and_then(|v| v.as_u64()) {
            if len > max_length as usize {
                errors.push(ValidationError {
                    path: path.to_string(),
                    expected: format!("maxLength {}", max_length),
                    actual: format!("length {}", len),
                    message: format!("String length {} exceeds maxLength {}", len, max_length),
                });
            }
        }

        // Check enum
        if let Some(enum_values) = schema.get("enum").and_then(|v| v.as_array()) {
            let matches = enum_values.iter().any(|ev| ev == value);
            if !matches {
                errors.push(ValidationError {
                    path: path.to_string(),
                    expected: format!("one of {}", enum_values.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")),
                    actual: s.to_string(),
                    message: format!("Value '{}' is not in the allowed enum list", s),
                });
            }
        }
    }

    // Number-specific validations
    if value.is_number() {
        let n = value.as_f64().unwrap_or(0.0);

        if let Some(minimum) = schema.get("minimum").and_then(|v| v.as_f64()) {
            if n < minimum {
                errors.push(ValidationError {
                    path: path.to_string(),
                    expected: format!("minimum {}", minimum),
                    actual: format!("{}", n),
                    message: format!("Value {} is less than minimum {}", n, minimum),
                });
            }
        }

        if let Some(maximum) = schema.get("maximum").and_then(|v| v.as_f64()) {
            if n > maximum {
                errors.push(ValidationError {
                    path: path.to_string(),
                    expected: format!("maximum {}", maximum),
                    actual: format!("{}", n),
                    message: format!("Value {} exceeds maximum {}", n, maximum),
                });
            }
        }

        // Check integer constraint
        if schema.get("type").and_then(|t| t.as_str()) == Some("integer") {
            if value.as_i64().is_none() {
                errors.push(ValidationError {
                    path: path.to_string(),
                    expected: "integer".to_string(),
                    actual: "float".to_string(),
                    message: "Value is not an integer".to_string(),
                });
            }
        }
    }

    // Array-specific validations
    if value.is_array() {
        // Validate items against sub-schema
        if let Some(items_schema) = schema.get("items") {
            let arr = value.as_array().unwrap();
            for (i, item) in arr.iter().enumerate() {
                let item_path = format!("{}[{}]", path, i);
                errors.extend(validate_schema_subset(item, items_schema, &item_path));
            }
        }
    }

    // Check enum for non-string types
    if !value.is_string() {
        if let Some(enum_values) = schema.get("enum").and_then(|v| v.as_array()) {
            let matches = enum_values.iter().any(|ev| ev == value);
            if !matches {
                errors.push(ValidationError {
                    path: path.to_string(),
                    expected: "one of enum values".to_string(),
                    actual: value.to_string(),
                    message: format!("Value is not in the allowed enum list"),
                });
            }
        }
    }

    errors
}

/// Determine the JSON type of a value.
fn json_type_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(n) => {
            if n.is_i64() { "integer" } else { "number" }
        },
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Check if an actual JSON type matches a schema type.
fn type_matches(actual: &str, expected: &str) -> bool {
    // "integer" is a subset of "number"
    if expected == "number" && actual == "integer" {
        return true;
    }
    actual == expected
}

// ─── build_retry_prompt ──────────────────────────────────────────────────────

/// Build a re-prompt message for ModelRetry — when structured output validation fails,
/// this generates the system message that is injected into the conversation to guide
/// the model's self-correction attempt.
///
/// The prompt follows PydanticAI's ModelRetry pattern:
/// - States what went wrong (validation errors)
/// - Shows the expected schema requirements
/// - Instructs the model to re-generate conforming output
pub fn build_retry_prompt(config: &StructuredOutputConfig, retry: &oneai_core::ModelRetry) -> String {
    let template = config.error_prompt_template.as_deref().unwrap_or(
        "Your previous output did not conform to the required JSON Schema.\n\
         Validation errors: {errors}\n\
         Expected schema: {schema_description}\n\
         Please re-generate your output as valid JSON that conforms to the schema.\n\
         Output ONLY the JSON, with no additional text or explanation."
    );

    // Replace template placeholders
    let prompt = template
        .replace("{errors}", &retry.error_message)
        .replace("{schema_description}", &schema_description(&retry.expected_schema))
        .replace("{retry_count}", &retry.retry_count.to_string());

    prompt
}

/// Generate a human-readable description of a JSON Schema.
fn schema_description(schema: &serde_json::Value) -> String {
    let type_str = schema.get("type").and_then(|t| t.as_str()).unwrap_or("any");

    let required = schema.get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", "))
        .unwrap_or_default();

    if required.is_empty() {
        format!("type: {}", type_str)
    } else {
        format!("type: {}, required fields: [{}]", type_str, required)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_valid_json_object() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name", "age"],
            "properties": {
                "name": { "type": "string", "minLength": 1 },
                "age": { "type": "integer", "minimum": 0 }
            }
        });

        let output = serde_json::json!({"name": "Alice", "age": 30}).to_string();
        let result = validate_json_schema(&output, &schema);
        assert!(result.passed);
        assert!(result.parsed_output.is_some());
    }

    #[test]
    fn test_validate_missing_required_property() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name", "age"],
            "properties": {
                "name": { "type": "string" },
                "age": { "type": "integer" }
            }
        });

        let output = serde_json::json!({"name": "Alice"}).to_string();
        let result = validate_json_schema(&output, &schema);
        assert!(!result.passed);
        assert!(result.errors.iter().any(|e| e.message.contains("missing")));
    }

    #[test]
    fn test_validate_wrong_type() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "age": { "type": "integer" }
            }
        });

        let output = serde_json::json!({"name": "Alice", "age": "not a number"}).to_string();
        let result = validate_json_schema(&output, &schema);
        assert!(!result.passed);
        assert!(result.errors.iter().any(|e| e.message.contains("Expected type")));
    }

    #[test]
    fn test_validate_non_json_output() {
        let schema = serde_json::json!({"type": "object"});
        let output = "This is just plain text, not JSON";
        let result = validate_json_schema(output, &schema);
        assert!(!result.passed);
        assert!(result.errors[0].message.contains("not valid JSON"));
    }

    #[test]
    fn test_validate_string_constraints() {
        let schema = serde_json::json!({
            "type": "string",
            "minLength": 3,
            "maxLength": 10
        });

        // Too short
        let result = validate_json_schema("\"ab\"", &schema);
        assert!(!result.passed);

        // Valid length
        let result = validate_json_schema("\"hello\"", &schema);
        assert!(result.passed);

        // Too long
        let result = validate_json_schema("\"this is way too long\"", &schema);
        assert!(!result.passed);
    }

    #[test]
    fn test_validate_enum() {
        let schema = serde_json::json!({
            "type": "string",
            "enum": ["plan", "react", "reflect", "explore"]
        });

        let result = validate_json_schema("\"react\"", &schema);
        assert!(result.passed);

        let result = validate_json_schema("\"unknown\"", &schema);
        assert!(!result.passed);
    }

    #[test]
    fn test_validate_array_items() {
        let schema = serde_json::json!({
            "type": "array",
            "items": { "type": "integer" }
        });

        let result = validate_json_schema("[1, 2, 3]", &schema);
        assert!(result.passed);

        let result = validate_json_schema("[1, \"two\", 3]", &schema);
        assert!(!result.passed);
    }

    #[test]
    fn test_build_retry_prompt() {
        let config = StructuredOutputConfig {
            schema: serde_json::json!({"type": "object", "required": ["answer"]}),
            max_retries: 3,
            re_prompt_on_failure: true,
            error_prompt_template: None,
        };
        let retry = oneai_core::ModelRetry {
            error_message: "Required property 'answer' is missing".to_string(),
            retry_count: 1,
            expected_schema: serde_json::json!({"type": "object", "required": ["answer"]}),
            failed_output: "some text".to_string(),
        };

        let prompt = build_retry_prompt(&config, &retry);
        assert!(prompt.contains("Required property 'answer' is missing"));
        assert!(prompt.contains("required fields: [answer]"));
    }

    #[test]
    fn test_validation_result_error_summary() {
        let errors = vec![
            ValidationError {
                path: "/name".to_string(),
                expected: "string".to_string(),
                actual: "integer".to_string(),
                message: "Wrong type".to_string(),
            },
            ValidationError {
                path: "/age".to_string(),
                expected: "required property".to_string(),
                actual: "missing".to_string(),
                message: "Missing property".to_string(),
            },
        ];
        let result = ValidationResult::failed(errors);
        let summary = result.error_summary();
        assert!(summary.contains("Wrong type"));
        assert!(summary.contains("Missing property"));
    }
}
