//! Agent loop — ports `_run_turn` (badbitch2.py:1647) and `_preflight` (1797).

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
                let result = run_tool(router, ctx, name, args.clone()).await;
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
        let content = if last_resp.message.content.is_empty() {
            "[no content]".to_string()
        } else {
            last_resp.message.content.clone()
        };
        debug::log(&format!(
            "assistant-final ({} chars): {}",
            content.chars().count(),
            truncate_chars(&content, 4000)
        ));
        return content;
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
