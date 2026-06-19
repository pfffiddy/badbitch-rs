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
