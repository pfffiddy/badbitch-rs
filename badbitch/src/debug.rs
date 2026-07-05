//! Per-run debug log — mirrors `_setup_debug_log` / `_log_chat_response` (badbitch2.py:136,
//! 1544). Truncated at session start; dumps raw model content (reveals text-leaked tool calls),
//! tool calls, and token telemetry — the signal for diagnosing a misbehaving model.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use crate::ollama::ChatResponse;

static LOG: LazyLock<Mutex<Option<File>>> = LazyLock::new(|| Mutex::new(None));

/// Truncate (overwrite) the debug log for a fresh session.
pub fn init(path: &Path) {
    if let Ok(f) = File::create(path) {
        *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }
}

pub fn log(msg: &str) {
    // Logging must never crash the agent — recover a poisoned lock rather than panicking.
    if let Some(f) = LOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = writeln!(f, "{}  {}", chrono::Utc::now().to_rfc3339(), msg);
    }
}

pub fn log_response(resp: &ChatResponse, phase: &str) {
    let m = &resp.message;
    log(&format!(
        "---- chat RESPONSE [{phase}] | role={} done_reason={} ----",
        m.role,
        resp.done_reason.as_deref().unwrap_or("?")
    ));
    if let Some(t) = &m.thinking {
        log(&format!("  thinking ({} chars): {t}", t.chars().count()));
    }
    log(&format!("  content ({} chars): {}", m.content.chars().count(), m.content));
    match &m.tool_calls {
        Some(tcs) if !tcs.is_empty() => {
            for (i, tc) in tcs.iter().enumerate() {
                log(&format!(
                    "  tool_call[{i}]: {} args={}",
                    tc.function.name, tc.function.arguments
                ));
            }
        }
        _ => log("  tool_calls: none"),
    }
    log(&format!(
        "  telemetry: prompt_eval_count={:?} eval_count={:?} total_duration={:?}",
        resp.prompt_eval_count, resp.eval_count, resp.total_duration
    ));
}

/// Append-only audit log of a tool invocation (`_audit`, badbitch2.py:241).
pub fn audit(log_file: &Path, tool: &str, args: &serde_json::Value) {
    if let Some(parent) = log_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_file) {
        let line = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "tool": tool,
            "args": args,
        })
        .to_string();
        let line: String = line.chars().take(2000).collect();
        let _ = writeln!(f, "{line}");
    }
}
