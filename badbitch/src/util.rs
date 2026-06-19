//! Small shared helpers (char-safe truncation, result clipping).

/// Truncate to at most `max` Unicode chars (Python slices by codepoint; naive byte slicing
/// would panic on multi-byte boundaries).
pub fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

/// `_clip_result` (badbitch2.py:1486): bound a single tool result before it enters history.
pub fn clip_result(result: &str, max: usize) -> String {
    if result.chars().count() <= max {
        return result.to_string();
    }
    format!(
        "{}\n…[tool output truncated to {max} chars]",
        truncate_chars(result, max)
    )
}

/// Truncate with a "…[truncated; N chars total]" marker, matching `_j` (badbitch2.py:213).
pub fn clip_marked(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    format!(
        "{}\n…[truncated; {total} chars total]",
        truncate_chars(s, max)
    )
}
