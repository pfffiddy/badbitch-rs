//! History compaction — ports `_compact` (badbitch2.py:1500). num_ctx-derived char-budget
//! tail (token-aware, no model call), never starting the retained tail on an orphan `tool`.

use crate::ollama::ChatMessage;

/// `_msg_chars` (badbitch2.py:1495): content length + per-message overhead.
fn msg_chars(m: &ChatMessage) -> usize {
    m.content.chars().count() + 150
}

const MARKER: &str = "[earlier context truncated to fit the window — rely on findings already stated; re-run a tool if you need a dropped detail.]";

pub fn compact(messages: &[ChatMessage], num_ctx: i64) -> Vec<ChatMessage> {
    if messages.len() <= 3 {
        return messages.to_vec();
    }
    // Reserve ~6k tokens for fixed tool schemas + the reply; ~3 chars/token for the rest.
    let budget = std::cmp::max(6000i64, (num_ctx - 6000) * 3) as usize;

    let head = &messages[0];
    let mut total = msg_chars(head);
    let mut kept: Vec<ChatMessage> = Vec::new();
    for m in messages[1..].iter().rev() {
        let s = msg_chars(m);
        if total + s > budget && kept.len() >= 2 {
            break;
        }
        total += s;
        kept.push(m.clone());
    }
    kept.reverse();

    while kept.first().map(|m| m.role == "tool").unwrap_or(false) {
        kept.remove(0); // don't lead with an orphan tool result
    }

    if kept.len() >= messages.len() - 1 {
        return messages.to_vec();
    }

    let mut out = Vec::with_capacity(kept.len() + 2);
    out.push(head.clone());
    out.push(ChatMessage::user(MARKER));
    out.extend(kept);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ollama::ChatMessage;

    #[test]
    fn keeps_all_when_small() {
        let msgs = vec![ChatMessage::system("s"), ChatMessage::user("u")];
        assert_eq!(compact(&msgs, 20480).len(), 2);
    }

    #[test]
    fn drops_old_keeps_system_and_marks_truncation() {
        let mut msgs = vec![ChatMessage::system("SYSTEM")];
        for _ in 0..200 {
            msgs.push(ChatMessage::user("x".repeat(500)));
        }
        let out = compact(&msgs, 8000); // small window forces compaction
        assert_eq!(out[0].role, "system");
        assert_eq!(out[0].content, "SYSTEM");
        assert!(out.len() < msgs.len());
        assert!(out.iter().any(|m| m.content.contains("truncated to fit")));
    }

    #[test]
    fn never_leads_with_orphan_tool() {
        let mut msgs = vec![ChatMessage::system("SYSTEM")];
        for _ in 0..200 {
            msgs.push(ChatMessage::simple("assistant", "a".repeat(300)));
            msgs.push(ChatMessage::tool_result("t", "r".repeat(300)));
        }
        let out = compact(&msgs, 8000);
        // The first kept message after the system + marker must not be an orphan tool result.
        assert!(out.get(2).map(|m| m.role != "tool").unwrap_or(true));
    }
}
