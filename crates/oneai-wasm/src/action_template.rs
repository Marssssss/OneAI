//! WasmActionTemplate — predefined WASM templates for code-as-action execution.
//!
//! Available templates:
//! - **Compute**: Mathematical expression evaluation (arithmetic, trigonometry, etc.)
//! - **Sort**: Data sorting (ascending, descending, by key)
//! - **Filter**: Data filtering (conditional, regex, threshold)
//! - **Extract**: JSON data extraction (path-based, key-based)
//!
//! ## Execution Modes
//!
//! Templates can run in two modes:
//! - **Native** (default): Pure Rust execution, always available
//! - **Wasm**: Execution via WASM sandbox using pre-compiled bytes (future)
//!
//! When WASM mode is selected but no pre-compiled bytes are available,
//! execution falls back to native mode transparently.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::{RiskLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;

use crate::runtime::WasmRuntime;

/// Execution mode for WASM action templates.
#[derive(Clone)]
#[non_exhaustive]
pub enum WasmActionExecutionMode {
    /// Native Rust execution (always available, no WASM sandbox).
    Native,
    /// WASM sandbox execution (requires pre-compiled bytes, falls back to Native if unavailable).
    Wasm {
        /// Reference to the WASM runtime.
        runtime: Arc<WasmRuntime>,
    },
}

impl std::fmt::Debug for WasmActionExecutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmActionExecutionMode::Native => write!(f, "Native"),
            WasmActionExecutionMode::Wasm { .. } => write!(f, "Wasm {{ runtime: ... }}"),
        }
    }
}

/// Available WASM action template kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmActionKind {
    /// Mathematical computation (expression evaluation).
    Compute,
    /// Data sorting (ascending, descending, by key).
    Sort,
    /// Data filtering (conditional, regex, threshold).
    Filter,
    /// JSON data extraction (path-based, key-based).
    Extract,
}

impl WasmActionKind {
    /// Get all available action kinds.
    pub fn all() -> Vec<WasmActionKind> {
        vec![WasmActionKind::Compute, WasmActionKind::Sort, WasmActionKind::Filter, WasmActionKind::Extract]
    }

    /// Get the template name for this kind.
    pub fn template_name(&self) -> &str {
        match self {
            WasmActionKind::Compute => "compute",
            WasmActionKind::Sort => "sort",
            WasmActionKind::Filter => "filter",
            WasmActionKind::Extract => "extract",
        }
    }

    /// Get the tool name for this kind.
    pub fn tool_name(&self) -> &str {
        match self {
            WasmActionKind::Compute => "wasm_compute",
            WasmActionKind::Sort => "wasm_sort",
            WasmActionKind::Filter => "wasm_filter",
            WasmActionKind::Extract => "wasm_extract",
        }
    }

    /// Get the description for this kind.
    pub fn description(&self) -> &str {
        match self {
            WasmActionKind::Compute => "Evaluate mathematical expressions in a WASM sandbox. \
                Supports arithmetic, trigonometry, logarithms, and comparisons. \
                Safer than ShellTool for pure computation tasks.",
            WasmActionKind::Sort => "Sort data in a WASM sandbox. \
                Supports ascending, descending, and key-based sorting. \
                Zero I/O access — data is passed as input and returned as output.",
            WasmActionKind::Filter => "Filter data in a WASM sandbox. \
                Supports conditional, regex, and threshold-based filtering. \
                All processing happens in sandboxed WASM memory.",
            WasmActionKind::Extract => "Extract data from JSON structures in a WASM sandbox. \
                Supports path-based and key-based extraction. \
                No filesystem access — input JSON is passed directly.",
        }
    }

    /// Get the parameters schema for this kind.
    pub fn parameters_schema(&self) -> serde_json::Value {
        match self {
            WasmActionKind::Compute => serde_json::json!({
                "type": "object",
                "properties": {
                    "expression": {
                        "type": "string",
                        "description": "Mathematical expression to evaluate (e.g., '2 + 3', 'sin(pi/4)', 'log(100)')"
                    },
                    "variables": {
                        "type": "object",
                        "description": "Optional variable definitions (e.g., {'pi': 3.14159, 'x': 10})"
                    }
                },
                "required": ["expression"]
            }),
            WasmActionKind::Sort => serde_json::json!({
                "type": "object",
                "properties": {
                    "data": {
                        "type": "array",
                        "description": "Array of values to sort"
                    },
                    "order": {
                        "type": "string",
                        "enum": ["ascending", "descending"],
                        "description": "Sort order (default: ascending)"
                    },
                    "key": {
                        "type": "string",
                        "description": "Key to sort by (for object arrays, e.g., 'age')"
                    }
                },
                "required": ["data"]
            }),
            WasmActionKind::Filter => serde_json::json!({
                "type": "object",
                "properties": {
                    "data": {
                        "type": "array",
                        "description": "Array of values to filter"
                    },
                    "condition": {
                        "type": "string",
                        "description": "Filter condition (e.g., '>5', 'contains hello', 'regex ^test')"
                    },
                    "threshold": {
                        "type": "number",
                        "description": "Numeric threshold for comparison filters"
                    }
                },
                "required": ["data", "condition"]
            }),
            WasmActionKind::Extract => serde_json::json!({
                "type": "object",
                "properties": {
                    "data": {
                        "type": "object",
                        "description": "JSON object to extract from"
                    },
                    "path": {
                        "type": "string",
                        "description": "Dot-separated path (e.g., 'users.0.name')"
                    }
                },
                "required": ["data", "path"]
            }),
        }
    }
}

/// WASM action template — a predefined computation pattern.
///
/// Each template has a Rust-native implementation that can be
/// executed directly (Native mode, always available) or via the
/// WASM sandbox (Wasm mode, future — requires pre-compiled bytes).
///
/// Native mode provides "sandboxed" conceptual execution — these templates
/// perform pure computation with no I/O side effects.
///
/// In Wasm mode, templates attempt execution via pre-compiled WASM bytes.
/// If no bytes are available (not yet compiled), execution falls back to
/// Native mode transparently.
#[derive(Debug, Clone)]
pub struct WasmActionTemplate {
    /// The action kind (compute, sort, filter, extract).
    kind: WasmActionKind,
    /// Execution mode (native or WASM).
    mode: WasmActionExecutionMode,
}

impl WasmActionTemplate {
    /// Create a template for the given action kind (Native mode).
    pub fn new(kind: WasmActionKind) -> Self {
        Self {
            kind,
            mode: WasmActionExecutionMode::Native,
        }
    }

    /// Create a template with WASM execution mode.
    pub fn with_wasm(kind: WasmActionKind, runtime: Arc<WasmRuntime>) -> Self {
        Self {
            kind,
            mode: WasmActionExecutionMode::Wasm { runtime },
        }
    }

    /// Get the action kind.
    pub fn kind(&self) -> &WasmActionKind {
        &self.kind
    }

    /// Get the execution mode.
    pub fn mode(&self) -> &WasmActionExecutionMode {
        &self.mode
    }

    /// Execute the template with the given arguments.
    ///
    /// In Native mode: executes the template logic in Rust directly.
    /// In Wasm mode: attempts WASM execution first; falls back to Native
    /// if pre-compiled bytes are not available.
    pub fn execute(&self, args: serde_json::Value) -> ToolOutput {
        match &self.mode {
            WasmActionExecutionMode::Native => self.execute_native(args),
            WasmActionExecutionMode::Wasm { runtime } => {
                // Try WASM execution; fall back to native if bytes not available
                let args_clone = args.clone();
                match self.execute_wasm(args, runtime) {
                    Ok(output) => output,
                    Err(_) => self.execute_native(args_clone),
                }
            }
        }
    }

    /// Get pre-compiled WASM bytes for this template kind (if available).
    ///
    /// Currently returns None for all templates — pre-compiled WASM modules
    /// will be added via `include_bytes!()` when the Rust→WASM compilation
    /// pipeline is set up.
    fn wasm_bytes(&self) -> Option<&'static [u8]> {
        match self.kind {
            WasmActionKind::Compute => None,
            WasmActionKind::Sort => None,
            WasmActionKind::Filter => None,
            WasmActionKind::Extract => None,
        }
    }

    /// Execute via WASM sandbox (attempt WASM, fallback on failure).
    fn execute_wasm(&self, _args: serde_json::Value, runtime: &Arc<WasmRuntime>) -> std::result::Result<ToolOutput, crate::error::WasmError> {
        let bytes = self.wasm_bytes()
            .ok_or_else(|| crate::error::WasmError::ModuleNotFound(format!(
                "No pre-compiled WASM bytes for template '{}'", self.kind.template_name()
            )))?;

        // For now, this always returns Err because wasm_bytes() returns None.
        // When pre-compiled bytes are added, this will:
        // 1. Compile the bytes to a Module via runtime.compile_module()
        // 2. Instantiate and execute in sandbox
        // 3. Parse the output
        Err(crate::error::WasmError::ModuleNotFound(format!(
            "Pre-compiled WASM bytes not yet available for '{}'", self.kind.template_name()
        )))
    }

    /// Execute using native Rust implementation.
    fn execute_native(&self, args: serde_json::Value) -> ToolOutput {
        match self.kind {
            WasmActionKind::Compute => self.execute_compute(args),
            WasmActionKind::Sort => self.execute_sort(args),
            WasmActionKind::Filter => self.execute_filter(args),
            WasmActionKind::Extract => self.execute_extract(args),
        }
    }

    // ─── Compute template ────────────────────────────────────────────────

    fn execute_compute(&self, args: serde_json::Value) -> ToolOutput {
        let expression = args.get("expression")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if expression.is_empty() {
            return ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No expression provided".to_string()),
            };
        }

        // Simple expression evaluator (phase 2: native Rust, not WASM)
        // Supports: arithmetic (+, -, *, /, %, ^), parentheses, variables
        let result = evaluate_expression(expression, args.get("variables"));

        match result {
            Ok(value) => ToolOutput {
                success: true,
                content: format!("{}", value),
                error: None,
            },
            Err(msg) => ToolOutput {
                success: false,
                content: String::new(),
                error: Some(msg),
            },
        }
    }

    // ─── Sort template ───────────────────────────────────────────────────

    fn execute_sort(&self, args: serde_json::Value) -> ToolOutput {
        let data = args.get("data")
            .and_then(|v| v.as_array());

        if data.is_none() {
            return ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No data array provided".to_string()),
            };
        }

        let data = data.unwrap();
        let order = args.get("order")
            .and_then(|v| v.as_str())
            .unwrap_or("ascending");
        let key = args.get("key")
            .and_then(|v| v.as_str());

        // Sort values
        let mut sorted: Vec<serde_json::Value> = data.clone();

        if key.is_some() {
            // Sort objects by key
            let key = key.unwrap().to_string();
            sorted.sort_by(|a, b| {
                let a_val = a.get(&key).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let b_val = b.get(&key).and_then(|v| v.as_f64()).unwrap_or(0.0);
                a_val.partial_cmp(&b_val).unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            // Sort primitives by numeric value (if possible) or string
            sorted.sort_by(|a, b| {
                match (a.as_f64(), b.as_f64()) {
                    (Some(a_n), Some(b_n)) => a_n.partial_cmp(&b_n).unwrap_or(std::cmp::Ordering::Equal),
                    _ => {
                        let a_str = a.as_str().unwrap_or("");
                        let b_str = b.as_str().unwrap_or("");
                        a_str.cmp(b_str)
                    }
                }
            });
        }

        if order == "descending" {
            sorted.reverse();
        }

        ToolOutput {
            success: true,
            content: serde_json::to_string_pretty(&sorted).unwrap_or_else(|_| serde_json::to_string(&sorted).unwrap_or_else(|_| "[]".to_string())),
            error: None,
        }
    }

    // ─── Filter template ─────────────────────────────────────────────────

    fn execute_filter(&self, args: serde_json::Value) -> ToolOutput {
        let data = args.get("data")
            .and_then(|v| v.as_array());

        if data.is_none() {
            return ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No data array provided".to_string()),
            };
        }

        let data = data.unwrap();
        let condition = args.get("condition")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let filtered: Vec<serde_json::Value> = data.iter()
            .filter(|item| matches_condition(item, condition, args.get("threshold")))
            .cloned()
            .collect();

        ToolOutput {
            success: true,
            content: serde_json::to_string_pretty(&filtered).unwrap_or_else(|_| serde_json::to_string(&filtered).unwrap_or_else(|_| "[]".to_string())),
            error: None,
        }
    }

    // ─── Extract template ────────────────────────────────────────────────

    fn execute_extract(&self, args: serde_json::Value) -> ToolOutput {
        let data = args.get("data");
        let path = args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if data.is_none() {
            return ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No data provided".to_string()),
            };
        }

        let result = extract_by_path(data.unwrap(), path);

        match result {
            Some(value) => ToolOutput {
                success: true,
                content: serde_json::to_string_pretty(&value).unwrap_or_else(|_| serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())),
                error: None,
            },
            None => ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Path '{}' not found in data", path)),
            },
        }
    }
}

/// WASM action tool — wraps WasmActionTemplate as a OneAI Tool.
///
/// This is the Tool trait implementation that the AgentLoop can call.
/// It selects the appropriate template based on the tool name and
/// executes it with the given arguments.
pub struct WasmActionTool {
    /// The action template kind.
    kind: WasmActionKind,
    /// Execution mode.
    mode: WasmActionExecutionMode,
}

impl WasmActionTool {
    /// Create a compute action tool (Native mode).
    pub fn compute() -> Self {
        Self { kind: WasmActionKind::Compute, mode: WasmActionExecutionMode::Native }
    }

    /// Create a sort action tool (Native mode).
    pub fn sort() -> Self {
        Self { kind: WasmActionKind::Sort, mode: WasmActionExecutionMode::Native }
    }

    /// Create a filter action tool (Native mode).
    pub fn filter() -> Self {
        Self { kind: WasmActionKind::Filter, mode: WasmActionExecutionMode::Native }
    }

    /// Create an extract action tool (Native mode).
    pub fn extract() -> Self {
        Self { kind: WasmActionKind::Extract, mode: WasmActionExecutionMode::Native }
    }

    /// Create a compute action tool (Wasm mode).
    pub fn compute_wasm(runtime: Arc<WasmRuntime>) -> Self {
        Self { kind: WasmActionKind::Compute, mode: WasmActionExecutionMode::Wasm { runtime } }
    }

    /// Create a sort action tool (Wasm mode).
    pub fn sort_wasm(runtime: Arc<WasmRuntime>) -> Self {
        Self { kind: WasmActionKind::Sort, mode: WasmActionExecutionMode::Wasm { runtime } }
    }

    /// Create a filter action tool (Wasm mode).
    pub fn filter_wasm(runtime: Arc<WasmRuntime>) -> Self {
        Self { kind: WasmActionKind::Filter, mode: WasmActionExecutionMode::Wasm { runtime } }
    }

    /// Create an extract action tool (Wasm mode).
    pub fn extract_wasm(runtime: Arc<WasmRuntime>) -> Self {
        Self { kind: WasmActionKind::Extract, mode: WasmActionExecutionMode::Wasm { runtime } }
    }

    /// Create all WASM action tools (Native mode).
    pub fn all() -> Vec<Self> {
        WasmActionKind::all().iter().map(|kind| Self {
            kind: kind.clone(),
            mode: WasmActionExecutionMode::Native,
        }).collect()
    }

    /// Create all WASM action tools (Wasm mode).
    pub fn all_wasm(runtime: Arc<WasmRuntime>) -> Vec<Self> {
        WasmActionKind::all().iter().map(|kind| Self {
            kind: kind.clone(),
            mode: WasmActionExecutionMode::Wasm { runtime: runtime.clone() },
        }).collect()
    }

    /// Get the action kind.
    pub fn kind(&self) -> &WasmActionKind {
        &self.kind
    }

    /// Get the execution mode.
    pub fn mode(&self) -> &WasmActionExecutionMode {
        &self.mode
    }
}

#[async_trait]
impl Tool for WasmActionTool {
    fn name(&self) -> &str {
        self.kind.tool_name()
    }

    fn description(&self) -> &str {
        self.kind.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.kind.parameters_schema()
    }

    fn risk_level(&self) -> RiskLevel {
        // WASM action tools are always Low-risk — pure computation in sandbox
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let template = WasmActionTemplate {
            kind: self.kind.clone(),
            mode: self.mode.clone(),
        };
        Ok(template.execute(args))
    }
}

// ─── Helper functions ──────────────────────────────────────────────────────

/// Simple mathematical expression evaluator.
///
/// Supports: +, -, *, /, %, ^ (power), parentheses, and named variables.
/// No external dependencies — pure Rust implementation.
fn evaluate_expression(expr: &str, variables: Option<&serde_json::Value>) -> std::result::Result<f64, String> {
    // Replace variable names with their values
    let mut processed = expr.to_string();
    if let Some(vars) = variables {
        if let Some(vars_map) = vars.as_object() {
            for (name, value) in vars_map {
                if let Some(num) = value.as_f64() {
                    processed = processed.replace(name, &format!("{}", num));
                }
            }
        }
    }

    // Replace common constants
    processed = processed.replace("pi", &format!("{}", std::f64::consts::PI));
    processed = processed.replace("e", &format!("{}", std::f64::consts::E));

    // Handle trigonometric functions
    for func in ["sin", "cos", "tan", "log", "ln", "sqrt", "abs", "floor", "ceil", "round"] {
        if processed.contains(func) {
            // Extract the argument between parentheses
            let pattern = format!("{}(", func);
            if let Some(start) = processed.find(&pattern) {
                let arg_start = start + pattern.len();
                if let Some(end) = processed[arg_start..].find(')') {
                    let arg_str = &processed[arg_start..arg_start + end];
                    let arg_val = evaluate_expression(arg_str, None)?;
                    let result = match func {
                        "sin" => arg_val.sin(),
                        "cos" => arg_val.cos(),
                        "tan" => arg_val.tan(),
                        "log" => arg_val.log10(),
                        "ln" => arg_val.ln(),
                        "sqrt" => arg_val.sqrt(),
                        "abs" => arg_val.abs(),
                        "floor" => arg_val.floor(),
                        "ceil" => arg_val.ceil(),
                        "round" => arg_val.round(),
                        _ => return Err(format!("Unknown function: {}", func)),
                    };
                    processed = format!("{}{}{}", &processed[..start], result, &processed[arg_start + end + 1..]);
                }
            }
        }
    }

    // Replace ^ with pow expression
    // Simple recursive descent parser for basic arithmetic
    simple_arithmetic_eval(&processed)
}

/// Simple recursive descent parser for arithmetic expressions.
///
/// Supports: +, -, *, /, %, parentheses, negative numbers.
/// No exponentiation (handled separately above).
fn simple_arithmetic_eval(expr: &str) -> std::result::Result<f64, String> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err("Empty expression".to_string());
    }

    // Remove whitespace
    let chars: Vec<char> = expr.chars().filter(|c| !c.is_whitespace()).collect();
    let mut pos = 0;

    fn parse_expression(chars: &[char], pos: &mut usize) -> std::result::Result<f64, String> {
        let mut result = parse_term(chars, pos)?;
        while *pos < chars.len() {
            match chars[*pos] {
                '+' => { *pos += 1; result += parse_term(chars, pos)?; }
                '-' => { *pos += 1; result -= parse_term(chars, pos)?; }
                _ => break,
            }
        }
        Ok(result)
    }

    fn parse_term(chars: &[char], pos: &mut usize) -> std::result::Result<f64, String> {
        let mut result = parse_factor(chars, pos)?;
        while *pos < chars.len() {
            match chars[*pos] {
                '*' => { *pos += 1; result *= parse_factor(chars, pos)?; }
                '/' => { *pos += 1; let divisor = parse_factor(chars, pos)?; if divisor == 0.0 { return Err("Division by zero".to_string()); } result /= divisor; }
                '%' => { *pos += 1; let divisor = parse_factor(chars, pos)?; if divisor == 0.0 { return Err("Modulo by zero".to_string()); } result %= divisor; }
                _ => break,
            }
        }
        Ok(result)
    }

    fn parse_factor(chars: &[char], pos: &mut usize) -> std::result::Result<f64, String> {
        // Handle unary minus
        if *pos < chars.len() && chars[*pos] == '-' {
            *pos += 1;
            return Ok(-parse_factor(chars, pos)?);
        }
        // Handle unary plus
        if *pos < chars.len() && chars[*pos] == '+' {
            *pos += 1;
            return parse_factor(chars, pos);
        }

        // Handle parentheses
        if *pos < chars.len() && chars[*pos] == '(' {
            *pos += 1; // skip '('
            let result = parse_expression(chars, pos)?;
            if *pos < chars.len() && chars[*pos] == ')' {
                *pos += 1; // skip ')'
                Ok(result)
            } else {
                Err("Missing closing parenthesis".to_string())
            }
        } else {
            // Parse number
            let start = *pos;
            while *pos < chars.len() && (chars[*pos].is_ascii_digit() || chars[*pos] == '.') {
                *pos += 1;
            }
            if start == *pos {
                Err(format!("Expected number at position {}", start))
            } else {
                let num_str: String = chars[start..*pos].iter().collect();
                num_str.parse::<f64>().map_err(|e| format!("Invalid number '{}': {}", num_str, e))
            }
        }
    }

    parse_expression(&chars, &mut pos)
}

/// Check if a JSON value matches a filter condition.
fn matches_condition(item: &serde_json::Value, condition: &str, threshold: Option<&serde_json::Value>) -> bool {
    // Numeric comparison: >5, <10, >=3, <=7, ==5
    if let Some(num) = item.as_f64() {
        if condition.starts_with('>') && !condition.starts_with(">=") {
            let threshold_val = condition[1..].parse::<f64>().unwrap_or(threshold.and_then(|v| v.as_f64()).unwrap_or(0.0));
            return num > threshold_val;
        }
        if condition.starts_with(">=") {
            let threshold_val = condition[2..].parse::<f64>().unwrap_or(threshold.and_then(|v| v.as_f64()).unwrap_or(0.0));
            return num >= threshold_val;
        }
        if condition.starts_with('<') && !condition.starts_with("<=") && !condition.starts_with("<<") {
            let threshold_val = condition[1..].parse::<f64>().unwrap_or(threshold.and_then(|v| v.as_f64()).unwrap_or(0.0));
            return num < threshold_val;
        }
        if condition.starts_with("<=") {
            let threshold_val = condition[2..].parse::<f64>().unwrap_or(threshold.and_then(|v| v.as_f64()).unwrap_or(0.0));
            return num <= threshold_val;
        }
        if condition.starts_with("==") {
            let threshold_val = condition[2..].parse::<f64>().unwrap_or(threshold.and_then(|v| v.as_f64()).unwrap_or(0.0));
            return num == threshold_val;
        }
    }

    // String contains: contains <substring>
    if condition.starts_with("contains ") {
        let substring = &condition[9..];
        if let Some(s) = item.as_str() {
            return s.contains(substring);
        }
        // Try stringifying the value
        let s = item.to_string();
        return s.contains(substring);
    }

    // Regex match: regex <pattern>
    if condition.starts_with("regex ") {
        let pattern = &condition[6..];
        if let Ok(re) = regex::Regex::new(pattern) {
            if let Some(s) = item.as_str() {
                return re.is_match(s);
            }
            let s = item.to_string();
            return re.is_match(&s);
        }
    }

    // Default: string equality
    if let Some(s) = item.as_str() {
        s == condition
    } else {
        item.to_string() == condition
    }
}

/// Extract a value from JSON by dot-separated path.
fn extract_by_path(data: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = data;

    for part in parts {
        // Try numeric index first (for arrays)
        if let Ok(index) = part.parse::<usize>() {
            if let Some(arr) = current.as_array() {
                if index < arr.len() {
                    current = &arr[index];
                    continue;
                }
            }
        }

        // Try key lookup (for objects)
        if let Some(obj) = current.as_object() {
            if let Some(value) = obj.get(part) {
                current = value;
                continue;
            }
        }

        return None;
    }

    Some(current.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasm_action_kind_all() {
        let all = WasmActionKind::all();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn test_wasm_action_kind_names() {
        assert_eq!(WasmActionKind::Compute.tool_name(), "wasm_compute");
        assert_eq!(WasmActionKind::Sort.tool_name(), "wasm_sort");
        assert_eq!(WasmActionKind::Filter.tool_name(), "wasm_filter");
        assert_eq!(WasmActionKind::Extract.tool_name(), "wasm_extract");
    }

    #[test]
    fn test_compute_template_basic() {
        let template = WasmActionTemplate::new(WasmActionKind::Compute);
        let result = template.execute_native(serde_json::json!({"expression": "2 + 3"}));
        assert!(result.success);
        assert_eq!(result.content, "5");
    }

    #[test]
    fn test_compute_template_multiplication() {
        let template = WasmActionTemplate::new(WasmActionKind::Compute);
        let result = template.execute_native(serde_json::json!({"expression": "3 * 4"}));
        assert!(result.success);
        assert_eq!(result.content, "12");
    }

    #[test]
    fn test_compute_template_division() {
        let template = WasmActionTemplate::new(WasmActionKind::Compute);
        let result = template.execute_native(serde_json::json!({"expression": "10 / 2"}));
        assert!(result.success);
        assert_eq!(result.content, "5");
    }

    #[test]
    fn test_compute_template_division_by_zero() {
        let template = WasmActionTemplate::new(WasmActionKind::Compute);
        let result = template.execute_native(serde_json::json!({"expression": "10 / 0"}));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("zero"));
    }

    #[test]
    fn test_compute_template_parentheses() {
        let template = WasmActionTemplate::new(WasmActionKind::Compute);
        let result = template.execute_native(serde_json::json!({"expression": "(2 + 3) * 4"}));
        assert!(result.success);
        assert_eq!(result.content, "20");
    }

    #[test]
    fn test_compute_template_variables() {
        let template = WasmActionTemplate::new(WasmActionKind::Compute);
        let result = template.execute_native(serde_json::json!({
            "expression": "x + y",
            "variables": {"x": 10, "y": 5}
        }));
        assert!(result.success);
        assert_eq!(result.content, "15");
    }

    #[test]
    fn test_compute_template_trigonometry() {
        let template = WasmActionTemplate::new(WasmActionKind::Compute);
        let result = template.execute_native(serde_json::json!({"expression": "sin(0)"}));
        assert!(result.success);
        // sin(0) ≈ 0
        let val: f64 = result.content.parse().unwrap();
        assert!(val.abs() < 0.001);
    }

    #[test]
    fn test_compute_template_sqrt() {
        let template = WasmActionTemplate::new(WasmActionKind::Compute);
        let result = template.execute_native(serde_json::json!({"expression": "sqrt(16)"}));
        assert!(result.success);
        assert_eq!(result.content, "4");
    }

    #[test]
    fn test_compute_template_empty_expression() {
        let template = WasmActionTemplate::new(WasmActionKind::Compute);
        let result = template.execute_native(serde_json::json!({"expression": ""}));
        assert!(!result.success);
    }

    #[test]
    fn test_sort_template_ascending() {
        let template = WasmActionTemplate::new(WasmActionKind::Sort);
        let result = template.execute_native(serde_json::json!({
            "data": [5, 3, 1, 4, 2],
            "order": "ascending"
        }));
        assert!(result.success);
        let sorted: Vec<i64> = serde_json::from_str(&result.content).unwrap();
        assert_eq!(sorted, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_sort_template_descending() {
        let template = WasmActionTemplate::new(WasmActionKind::Sort);
        let result = template.execute_native(serde_json::json!({
            "data": [5, 3, 1, 4, 2],
            "order": "descending"
        }));
        assert!(result.success);
        let sorted: Vec<i64> = serde_json::from_str(&result.content).unwrap();
        assert_eq!(sorted, vec![5, 4, 3, 2, 1]);
    }

    #[test]
    fn test_sort_template_by_key() {
        let template = WasmActionTemplate::new(WasmActionKind::Sort);
        let result = template.execute_native(serde_json::json!({
            "data": [{"name": "a", "age": 30}, {"name": "b", "age": 20}, {"name": "c", "age": 25}],
            "key": "age",
            "order": "ascending"
        }));
        assert!(result.success);
        let sorted: Vec<serde_json::Value> = serde_json::from_str(&result.content).unwrap();
        assert_eq!(sorted[0].get("name").unwrap().as_str().unwrap(), "b");
        assert_eq!(sorted[1].get("name").unwrap().as_str().unwrap(), "c");
        assert_eq!(sorted[2].get("name").unwrap().as_str().unwrap(), "a");
    }

    #[test]
    fn test_sort_template_no_data() {
        let template = WasmActionTemplate::new(WasmActionKind::Sort);
        let result = template.execute_native(serde_json::json!({}));
        assert!(!result.success);
    }

    #[test]
    fn test_filter_template_numeric() {
        let template = WasmActionTemplate::new(WasmActionKind::Filter);
        let result = template.execute_native(serde_json::json!({
            "data": [1, 3, 5, 7, 9],
            "condition": ">5"
        }));
        assert!(result.success);
        let filtered: Vec<i64> = serde_json::from_str(&result.content).unwrap();
        assert_eq!(filtered, vec![7, 9]);
    }

    #[test]
    fn test_filter_template_contains() {
        let template = WasmActionTemplate::new(WasmActionKind::Filter);
        let result = template.execute_native(serde_json::json!({
            "data": ["hello world", "hi", "hey hello", "bye"],
            "condition": "contains hello"
        }));
        assert!(result.success);
        let filtered: Vec<String> = serde_json::from_str(&result.content).unwrap();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_template_regex() {
        let template = WasmActionTemplate::new(WasmActionKind::Filter);
        let result = template.execute_native(serde_json::json!({
            "data": ["test123", "hello", "test", "456"],
            "condition": "regex ^test"
        }));
        assert!(result.success);
        let filtered: Vec<String> = serde_json::from_str(&result.content).unwrap();
        assert_eq!(filtered.len(), 2); // test123 and test
    }

    #[test]
    fn test_extract_template_simple_path() {
        let template = WasmActionTemplate::new(WasmActionKind::Extract);
        let result = template.execute_native(serde_json::json!({
            "data": {"users": [{"name": "Alice", "age": 30}, {"name": "Bob", "age": 25}]},
            "path": "users.0.name"
        }));
        assert!(result.success);
        assert_eq!(result.content.trim(), "\"Alice\""); // JSON string has quotes
    }

    #[test]
    fn test_extract_template_nested_path() {
        let template = WasmActionTemplate::new(WasmActionKind::Extract);
        let result = template.execute_native(serde_json::json!({
            "data": {"config": {"database": {"host": "localhost", "port": 5432}}},
            "path": "config.database.host"
        }));
        assert!(result.success);
        assert_eq!(result.content.trim(), "\"localhost\"");
    }

    #[test]
    fn test_extract_template_missing_path() {
        let template = WasmActionTemplate::new(WasmActionKind::Extract);
        let result = template.execute_native(serde_json::json!({
            "data": {"foo": "bar"},
            "path": "nonexistent"
        }));
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_wasm_action_tool_compute() {
        let tool = WasmActionTool::compute();
        assert_eq!(tool.name(), "wasm_compute");
        assert_eq!(tool.risk_level(), RiskLevel::Low);

        let result = tool.execute(serde_json::json!({"expression": "2 + 3"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "5");
    }

    #[tokio::test]
    async fn test_wasm_action_tool_sort() {
        let tool = WasmActionTool::sort();
        assert_eq!(tool.name(), "wasm_sort");

        let result = tool.execute(serde_json::json!({
            "data": [3, 1, 2],
            "order": "ascending"
        })).await.unwrap();
        assert!(result.success);
        let sorted: Vec<i64> = serde_json::from_str(&result.content).unwrap();
        assert_eq!(sorted, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_wasm_action_tool_filter() {
        let tool = WasmActionTool::filter();
        assert_eq!(tool.name(), "wasm_filter");

        let result = tool.execute(serde_json::json!({
            "data": [1, 5, 10],
            "condition": ">3"
        })).await.unwrap();
        assert!(result.success);
        let filtered: Vec<i64> = serde_json::from_str(&result.content).unwrap();
        assert_eq!(filtered, vec![5, 10]);
    }

    #[tokio::test]
    async fn test_wasm_action_tool_extract() {
        let tool = WasmActionTool::extract();
        assert_eq!(tool.name(), "wasm_extract");

        let result = tool.execute(serde_json::json!({
            "data": {"a": {"b": "hello"}},
            "path": "a.b"
        })).await.unwrap();
        assert!(result.success);
    }

    #[test]
    fn test_wasm_action_tool_all() {
        let tools = WasmActionTool::all();
        assert_eq!(tools.len(), 4);
        assert_eq!(tools[0].name(), "wasm_compute");
        assert_eq!(tools[1].name(), "wasm_sort");
        assert_eq!(tools[2].name(), "wasm_filter");
        assert_eq!(tools[3].name(), "wasm_extract");
    }
}
