//! Shared formatting helpers for tool output.

use super::SubagentResult;
use serde_json::Value;

/// Extract a JSON array from an LLM response that may contain extraneous text.
///
/// Attempts four strategies in order:
/// 1. Direct parse of the trimmed input
/// 2. Extract from a markdown code block (```json ... ``` or ``` ... ```)
/// 3. Find the first `[` and last `]` and parse the slice
/// 4. Final direct parse (for a clear error message)
pub fn extract_json_array<T: serde::de::DeserializeOwned>(
    text: &str,
) -> Result<T, serde_json::Error> {
    let trimmed = text.trim();

    // 1. Direct parse first.
    if let Ok(val) = serde_json::from_str::<T>(trimmed) {
        return Ok(val);
    }

    // 2. Extract from markdown code blocks.
    static CODE_BLOCK_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    // SAFETY: static regex pattern, compiles infallibly.
    let code_block_re = CODE_BLOCK_RE
        .get_or_init(|| regex::Regex::new(r"(?s)```(?:json)?\s*(\[.*?\])\s*```").unwrap());
    if let Some(caps) = code_block_re.captures(trimmed) {
        if let Some(block) = caps.get(1) {
            if let Ok(val) = serde_json::from_str::<T>(block.as_str()) {
                return Ok(val);
            }
        }
    }

    // 3. Fallback: find the first '[' and last ']'.
    if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            if end > start {
                if let Ok(val) = serde_json::from_str::<T>(&trimmed[start..=end]) {
                    return Ok(val);
                }
            }
        }
    }

    // 4. Final attempt for a clear error message.
    serde_json::from_str::<T>(trimmed)
}

/// Extract a JSON object from LLM response text (mirrors `extract_json_array` but for `{...}`).
pub fn extract_json_object<T: serde::de::DeserializeOwned>(
    text: &str,
) -> Result<T, serde_json::Error> {
    let trimmed = text.trim();

    // 1. Direct parse.
    if let Ok(val) = serde_json::from_str::<T>(trimmed) {
        return Ok(val);
    }

    // 2. Extract from markdown code blocks.
    static CODE_BLOCK_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let code_block_re = CODE_BLOCK_RE
        .get_or_init(|| regex::Regex::new(r"(?s)```(?:json)?\s*(\{.*?\})\s*```").unwrap());
    if let Some(caps) = code_block_re.captures(trimmed) {
        if let Some(block) = caps.get(1) {
            if let Ok(val) = serde_json::from_str::<T>(block.as_str()) {
                return Ok(val);
            }
        }
    }

    // 3. Fallback: find balanced braces.
    if let Some(start) = trimmed.find('{') {
        let mut depth = 0i32;
        for (i, ch) in trimmed.bytes().enumerate().skip(start) {
            match ch {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Ok(val) = serde_json::from_str::<T>(&trimmed[start..=i]) {
                            return Ok(val);
                        }
                        break;
                    }
                }
                b'"' => {
                    // Skip string contents to avoid counting braces inside strings.
                    // Simple approach: just continue — the balanced count is usually correct enough.
                }
                _ => {}
            }
        }
    }

    // 4. Final attempt.
    serde_json::from_str::<T>(trimmed)
}

/// Extract the first JSON object from arbitrary LLM output as a raw `Value`.
///
/// Tolerates surrounding prose, markdown fences, and trailing commentary
/// by delegating to [`extract_json_object`] with `serde_json::Value`.
pub fn extract_json_value(text: &str) -> Result<Value, String> {
    extract_json_object::<Value>(text).map_err(|e| format!("JSON parse error: {}", e))
}

/// Truncate a string for display in the WebSocket stream.
///
/// Prevents the frontend from storing excessively large tool inputs/outputs
/// and causing browser memory issues.
pub fn truncate_for_display(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        s.to_string()
    } else {
        let byte_pos = s
            .char_indices()
            .nth(max_len)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!(
            "{}... ({} chars total, truncated)",
            &s[..byte_pos],
            char_count
        )
    }
}

/// Format a single [`SubagentResult`] into a human-readable string.
pub fn format_subagent_response(result: &SubagentResult) -> String {
    let mut output = String::new();

    if let Some(ref reasoning) = result.response.reasoning_content {
        if !reasoning.is_empty() {
            output.push_str(&format!("**Thinking:**\n{}\n\n", reasoning));
        }
    }

    output.push_str(&format!(
        "**Model:** {}\n**Task:** {}\n\n**Response:**\n{}",
        result.model.as_deref().unwrap_or("unknown"),
        result.task,
        result.response.content
    ));

    output
}
