//! Agent loop — ports `_run_turn` (badbitch2.py:1647), `_preflight` (1797), and
//! `_summarize_turn` (badbitch2.py:1601).

use std::time::Instant;

use serde_json::Value;

use crate::compact::compact;
use crate::config::Config;
use crate::debug;
use crate::ollama::{ChatMessage, OllamaClient};
use crate::recovery_calls::get_tool_calls;
use crate::tool::web::{ReconSweepInput, recon_sweep};
use crate::tool::{ToolContext, ToolRouter};
use crate::util::{clip_result, truncate_chars};

fn shown(args: &Value) -> String {
    match args.as_object() {
        Some(map) => map
            .iter()
            .map(|(k, v)| {
                let s = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let s = if s.chars().count() > 60 {
                    format!("{}…", truncate_chars(&s, 60))
                } else {
                    s
                };
                format!("{k}: {s}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => args.to_string(),
    }
}

async fn run_tool(router: &ToolRouter, ctx: &ToolContext, name: &str, args: Value) -> String {
    if !router.contains(name) {
        return format!("[unknown tool] {name}");
    }
    match router.call(ctx, name, args).await {
        Ok(s) => s,
        Err(e) => format!("[tool error] {name}: {e}"),
    }
}

/// `_summarize_turn` (badbitch2.py:1601): one extra model call that distills the answer into
/// a 3-5 bullet TL;DR header. Returns "" on failure or when the answer is too short/trivial.
async fn summarize_turn(client: &OllamaClient, cfg: &Config, content: &str) -> String {
    if !cfg.summarize || content.chars().count() < 400 || content.starts_with('[') {
        return String::new();
    }
    let sm = vec![
        ChatMessage::system(
            "You compress an OSINT answer into a TL;DR. Output ONLY 3-5 terse bullet lines, \
             each starting with '- ', capturing the key findings and any explicit gaps. \
             No preamble, no heading, no closing remark. Keep each bullet under ~20 words.",
        ),
        ChatMessage::user(crate::util::truncate_chars(content, 6000)),
    ];
    let resp = match client.chat_no_tools(&sm, cfg, 0.2).await {
        Ok(r) => r,
        Err(e) => {
            debug::log(&format!("summary pass failed (non-fatal): {e}"));
            return String::new();
        }
    };
    let tldr = resp.message.content.trim().to_string();
    if tldr.is_empty() {
        return String::new();
    }
    let bullets: Vec<String> = tldr
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .map(|l| if l.starts_with(['-', '•', '*']) { l } else { format!("- {l}") })
        .take(5)
        .collect();
    if bullets.is_empty() {
        return String::new();
    }
    format!("═══ TL;DR ═══\n{}\n═════════════\n\n", bullets.join("\n"))
}

/// Forced finalization: when the model won't stop calling tools (caps exhausted) or its final
/// message is empty — classically because its last act was `save_dossier(...)`, so the whole
/// write-up went into the tool arguments — make ONE tool-free call that forces it to state its
/// findings. Without this the user sees `[no content]` after a full investigation. Returns None
/// on failure (caller falls back to whatever content it had).
async fn finalize(client: &OllamaClient, cfg: &Config, messages: &[ChatMessage]) -> Option<String> {
    let mut msgs = messages.to_vec();
    msgs.push(ChatMessage::user(
        "You are wrapping up this turn — do NOT request any more tools. Using ONLY what you \
         have already gathered (including any dossier you just saved), write your final answer \
         to the user NOW as a Markdown dossier: state what you found, cite a source for each \
         fact, and clearly flag any gaps. Respond in English.",
    ));
    match client.chat_no_tools(&msgs, cfg, cfg.gen_temp).await {
        Ok(r) if !r.message.content.trim().is_empty() => Some(r.message.content),
        Ok(_) => None,
        Err(e) => {
            debug::log(&format!("finalize pass failed (non-fatal): {e}"));
            None
        }
    }
}

/// `_run_turn`: model + tool loop with iteration cap and optional continuations.
pub async fn run_turn(
    client: &OllamaClient,
    router: &ToolRouter,
    ctx: &ToolContext,
    cfg: &Config,
    messages: &mut Vec<ChatMessage>,
) -> String {
    let specs = router.tool_specs();
    let mut continuations = 0usize;
    let turn_t0 = Instant::now();
    let mut tool_count = 0usize;

    loop {
        debug::log(&format!(
            "==== chat REQUEST [turn] msgs={} tools={} ====",
            messages.len(),
            specs.len()
        ));
        let resp = match client.chat(messages, &specs).await {
            Ok(r) => r,
            Err(e) => {
                debug::log(&format!("ollama.chat failed: {e}"));
                return format!("[ollama error] {e}");
            }
        };
        debug::log_response(&resp, "turn-start");
        let (mut tcs, mut recovered) = get_tool_calls(&resp);
        let mut last_resp = resp;

        let mut iters = 0usize;
        while !tcs.is_empty() && iters < cfg.max_tool_iters {
            iters += 1;
            messages.push(last_resp.message.clone());
            if recovered {
                println!(
                    "  [recovered {} tool-call(s) from text — model lacks native tool_calls]",
                    tcs.len()
                );
                debug::log(&format!("recovered {} tool-call(s) from text content", tcs.len()));
            }
            for tc in &tcs {
                let name = &tc.function.name;
                let args = &tc.function.arguments;
                println!("  → {}({})", name, shown(args));
                debug::audit(&cfg.log_file, name, args);
                debug::log(&format!("tool-call #{iters} {name} args={args}"));
                let t0 = Instant::now();
                let result = run_tool(router, ctx, name, args.clone()).await;
                tool_count += 1;
                if cfg.verbose {
                    println!("    ⏱  {name} took {:.1}s", t0.elapsed().as_secs_f64());
                }
                debug::log(&format!("tool-result {name} -> {}", truncate_chars(&result, 2000)));
                messages.push(ChatMessage::tool_result(
                    name,
                    clip_result(&result, cfg.max_tool_result_chars),
                ));
            }
            *messages = compact(messages, cfg.num_ctx);

            let resp = match client.chat(messages, &specs).await {
                Ok(r) => r,
                Err(e) => {
                    debug::log(&format!("ollama.chat (mid-loop) failed: {e}"));
                    return format!("[ollama error mid-loop] {e}");
                }
            };
            debug::log_response(&resp, &format!("mid-loop iter={iters}"));
            let g = get_tool_calls(&resp);
            tcs = g.0;
            recovered = g.1;
            last_resp = resp;
        }

        // Hit the cap but the model still wants tools -> bounded continuation.
        if !tcs.is_empty() && continuations < cfg.max_continuations {
            continuations += 1;
            println!(
                "  [hit {}-tool cap — continuation {}/{}]",
                cfg.max_tool_iters, continuations, cfg.max_continuations
            );
            messages.push(last_resp.message.clone());
            for tc in &tcs {
                let name = &tc.function.name;
                let args = &tc.function.arguments;
                debug::audit(&cfg.log_file, name, args);
                debug::log(&format!("tool-call (continuation {continuations}) {name} args={args}"));
                let result = run_tool(router, ctx, name, args.clone()).await;
                tool_count += 1;
                debug::log(&format!("tool-result {name} -> {}", truncate_chars(&result, 2000)));
                messages.push(ChatMessage::tool_result(
                    name,
                    clip_result(&result, cfg.max_tool_result_chars),
                ));
            }
            *messages = compact(messages, cfg.num_ctx);
            continue;
        }

        messages.push(last_resp.message.clone());
        // `tcs` still non-empty here means we exhausted continuations with the model still
        // wanting tools; an empty final message usually means the write-up went into a
        // save_dossier(...) call. Either way, force a tool-free wrap-up so the user gets output.
        let capped_out = !tcs.is_empty();
        let mut content = last_resp.message.content.clone();
        if content.trim().is_empty() || capped_out {
            if capped_out {
                println!("  [tool limit reached — asking the model to write up its findings]");
            }
            if let Some(text) = finalize(client, cfg, messages).await {
                messages.push(ChatMessage::simple("assistant", text.clone()));
                content = text;
            }
        }
        if content.trim().is_empty() {
            content = "[no content]".to_string();
        }
        debug::log(&format!(
            "assistant-final ({} chars): {}",
            content.chars().count(),
            truncate_chars(&content, 4000)
        ));
        if cfg.verbose {
            println!(
                "  [turn done: {tool_count} tool call(s) in {:.1}s]",
                turn_t0.elapsed().as_secs_f64()
            );
        }
        let tldr = summarize_turn(client, cfg, &content).await;
        return format!("{tldr}{content}");
    }
}

/// `_preflight`: build a recon corpus before the model's first turn and inject the digest.
pub async fn preflight(ctx: &ToolContext, cfg: &Config, messages: &mut Vec<ChatMessage>, query: &str) {
    if !cfg.prefetch_recon {
        return;
    }
    if messages.iter().any(|m| m.role == "user") {
        return;
    }
    println!("  [pre-flight recon — gathering a corpus before the model runs…]");
    let digest = recon_sweep(
        ctx.clone(),
        ReconSweepInput {
            target: query.to_string(),
            location: String::new(),
            max_docs: 6,
        },
    )
    .await;
    debug::log(&format!("preflight recon digest:\n{digest}"));
    println!("{digest}\n");
    messages.push(ChatMessage::user(format!(
        "[PRE-FETCH RECON CORPUS — archived before you started; the named subject is primary. Mine it with query_docs('pattern') / read_doc(id) before fetching anything new.]\n{digest}"
    )));
}
