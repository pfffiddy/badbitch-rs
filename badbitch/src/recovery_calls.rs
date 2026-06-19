//! Tool-call recovery — ports `_TOOLCALL_PATTERNS` (1580), `_parse_tool_calls_from_text`
//! (1597) and `_get_tool_calls` (1627).
//!
//! The abliterated GGUF often emits tool calls as *text* in `message.content` instead of the
//! structured `tool_calls` field. We recover all four wrapper styles so any model works; the
//! first wrapper style that matches wins (avoid double-executing).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::ollama::{ChatResponse, FunctionCall, ToolCall};

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?is)<tool_call>\s*(\{.*?\})\s*</tool_call>").unwrap(),
        Regex::new(r"(?is)<TOOLCALL>\s*(\[.*?\]|\{.*?\})\s*</TOOLCALL>").unwrap(),
        Regex::new(r"(?is)<function_call>\s*(\{.*?\})\s*</function_call>").unwrap(),
        Regex::new(r#"(?s)```(?:json|tool_call|tool_code)?\s*(\{[^`]*?"name"[^`]*?\})\s*```"#).unwrap(),
    ]
});

/// Recover tool calls a model emitted as text. Returns a list of (name, args_object).
pub fn parse_tool_calls_from_text(content: &str) -> Vec<(String, Value)> {
    if content.is_empty() {
        return vec![];
    }
    let mut found: Vec<(String, Value)> = Vec::new();
    for pat in PATTERNS.iter() {
        for caps in pat.captures_iter(content) {
            let Some(raw) = caps.get(1) else { continue };
            let Ok(data) = serde_json::from_str::<Value>(raw.as_str().trim()) else {
                continue;
            };
            let items: Vec<Value> = match data {
                Value::Array(a) => a,
                other => vec![other],
            };
            for it in items {
                let Some(obj) = it.as_object() else { continue };
                let name = obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .or_else(|| obj.get("function").and_then(|v| v.as_str()));
                let Some(name) = name else { continue };

                let mut args = obj.get("arguments").cloned();
                if matches!(args, None | Some(Value::Null)) {
                    args = obj.get("parameters").cloned();
                }
                let args_val = match args {
                    Some(Value::String(s)) => serde_json::from_str::<Value>(&s)
                        .ok()
                        .filter(|v| v.is_object())
                        .unwrap_or_else(|| json!({})),
                    Some(v) if v.is_object() => v,
                    _ => json!({}),
                };
                found.push((name.to_string(), args_val));
            }
        }
        if !found.is_empty() {
            break; // first wrapper style that matches wins
        }
    }
    found
}

/// `_get_tool_calls` (badbitch2.py:1627): prefer native `tool_calls`; else recover from text.
/// Returns (calls, recovered_from_text).
pub fn get_tool_calls(resp: &ChatResponse) -> (Vec<ToolCall>, bool) {
    if let Some(native) = &resp.message.tool_calls
        && !native.is_empty()
    {
        return (native.clone(), false);
    }
    let recovered = parse_tool_calls_from_text(&resp.message.content);
    if recovered.is_empty() {
        return (vec![], false);
    }
    let calls = recovered
        .into_iter()
        .map(|(name, arguments)| ToolCall {
            function: FunctionCall { name, arguments },
        })
        .collect();
    (calls, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_tool_call_tag() {
        let s = r#"sure<tool_call>{"name": "web_search", "arguments": {"query": "midland tx"}}</tool_call>"#;
        let calls = parse_tool_calls_from_text(s);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "web_search");
        assert_eq!(calls[0].1["query"], "midland tx");
    }

    #[test]
    fn nemotron_toolcall_array() {
        let s = r#"<TOOLCALL>[{"name": "geocode", "arguments": {"address": "16303 N Aster"}}]</TOOLCALL>"#;
        let calls = parse_tool_calls_from_text(s);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "geocode");
        assert_eq!(calls[0].1["address"], "16303 N Aster");
    }

    #[test]
    fn function_call_tag_with_parameters_key() {
        let s = r#"<function_call>{"name": "wayback", "parameters": {"url": "http://x.com"}}</function_call>"#;
        let calls = parse_tool_calls_from_text(s);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "wayback");
        assert_eq!(calls[0].1["url"], "http://x.com");
    }

    #[test]
    fn fenced_json_block() {
        let s = "here you go\n```json\n{\"name\": \"sherlock\", \"arguments\": {\"username\": \"acidburn\"}}\n```\n";
        let calls = parse_tool_calls_from_text(s);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sherlock");
        assert_eq!(calls[0].1["username"], "acidburn");
    }

    #[test]
    fn arguments_as_json_string() {
        let s = r#"<tool_call>{"name": "web_search", "arguments": "{\"query\": \"x\"}"}</tool_call>"#;
        let calls = parse_tool_calls_from_text(s);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["query"], "x");
    }

    #[test]
    fn missing_arguments_defaults_to_empty_object() {
        let s = r#"<tool_call>{"name": "recon_sweep"}</tool_call>"#;
        let calls = parse_tool_calls_from_text(s);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "recon_sweep");
        assert!(calls[0].1.is_object());
    }

    #[test]
    fn first_wrapper_wins_no_double_execute() {
        // Same call expressed two ways — only the first matched wrapper should be returned.
        let s = "<tool_call>{\"name\": \"a\", \"arguments\": {}}</tool_call>\n```json\n{\"name\": \"a\"}\n```";
        let calls = parse_tool_calls_from_text(s);
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn plain_text_yields_nothing() {
        assert!(parse_tool_calls_from_text("just a normal answer, no tools").is_empty());
    }
}
