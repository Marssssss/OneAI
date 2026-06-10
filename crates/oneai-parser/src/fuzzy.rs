//! Fuzzy JSON repair engine — Layer 2 of the 3-layer parsing defense.
//!
//! Repairs malformed JSON output from LLM models:
//! - Closing unclosed brackets/braces
//! - Extracting JSON embedded in non-JSON text using regex
//! - Handling truncated or partial JSON

use oneai_core::error::ParserError;
use oneai_core::OneAIError;
use regex::Regex;
use serde_json::Value;

/// The fuzzy JSON repair engine.
pub struct FuzzyJsonRepair {
    /// Regex patterns for extracting JSON from text.
    json_extract_patterns: Vec<Regex>,
}

impl FuzzyJsonRepair {
    /// Create a new fuzzy repair engine with default patterns.
    pub fn new() -> Self {
        let patterns = vec![
            // Extract JSON object from text: find content between { and }
            Regex::new(r"\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}").unwrap(),
            // Extract JSON array from text: find content between [ and ]
            Regex::new(r"\[[^\[\]]*(?:\[[^\[\]]*\][^\[\]]*)*\]").unwrap(),
        ];

        Self {
            json_extract_patterns: patterns,
        }
    }

    /// Attempt to repair and parse a raw string as JSON.
    ///
    /// Returns the parsed JSON value if repair succeeds, or a ParserError if it fails.
    pub fn repair_and_parse(&self, raw: &str) -> std::result::Result<Value, OneAIError> {
        // Strategy 1: Try direct parse first
        if let Ok(val) = serde_json::from_str::<Value>(raw) {
            return Ok(val);
        }

        // Strategy 2: Try closing unclosed brackets
        let repaired = self.close_brackets(raw);
        if let Ok(val) = serde_json::from_str::<Value>(&repaired) {
            return Ok(val);
        }

        // Strategy 3: Extract JSON from surrounding text using regex
        for pattern in &self.json_extract_patterns {
            if let Some(match_str) = pattern.find(raw).map(|m| m.as_str()) {
                // Try parsing the extracted fragment
                if let Ok(val) = serde_json::from_str::<Value>(match_str) {
                    return Ok(val);
                }
                // Try closing brackets on the extracted fragment
                let repaired_fragment = self.close_brackets(match_str);
                if let Ok(val) = serde_json::from_str::<Value>(&repaired_fragment) {
                    return Ok(val);
                }
            }
        }

        // Strategy 4: Remove trailing content after the last closing brace
        let trimmed = self.trim_trailing_content(raw);
        if let Ok(val) = serde_json::from_str::<Value>(&trimmed) {
            return Ok(val);
        }

        Err(OneAIError::Parser(ParserError::FuzzyRepairFailed(
            format!("Could not repair JSON from: {}", &raw[..raw.len().min(200)]),
        )))
    }

    /// Close unclosed brackets and braces in a JSON string.
    fn close_brackets(&self, raw: &str) -> String {
        let mut open_braces = 0;
        let mut open_brackets = 0;

        for ch in raw.chars() {
            match ch {
                '{' => open_braces += 1,
                '}' if open_braces > 0 => open_braces -= 1,
                '[' => open_brackets += 1,
                ']' if open_brackets > 0 => open_brackets -= 1,
                _ => {}
            }
        }

        let mut result = raw.to_string();
        for _ in 0..open_brackets {
            result.push(']');
        }
        for _ in 0..open_braces {
            result.push('}');
        }
        result
    }

    /// Trim trailing content after the last closing brace.
    fn trim_trailing_content(&self, raw: &str) -> String {
        if let Some(pos) = raw.rfind('}') {
            raw[..pos + 1].to_string()
        } else if let Some(pos) = raw.rfind(']') {
            raw[..pos + 1].to_string()
        } else {
            raw.to_string()
        }
    }
}

impl Default for FuzzyJsonRepair {
    fn default() -> Self {
        Self::new()
    }
}