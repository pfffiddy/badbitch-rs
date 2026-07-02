//! Agent loop — ports `_run_turn` (badbitch2.py:1647), `_preflight` (1797), and
//! `_summarize_turn` (badbitch2.py:1601).

use std::sync::mpsc::Sender;
use std::time::Instant;

use serde_json::Value;

use crate::compact::compact;
use crate::config::Config;
use crate::debug;
use crate::ollama::{ChatMessage, ChatResponse, OllamaClient};
use crate::recovery_calls::get_tool_calls;
use crate::tool::web::{ReconSweepInput, recon_sweep};
use crate::tool::{ToolContext, ToolRouter};
use crate::util::{clip_result, truncate_chars};

/// Live events emitted during a turn, so a UI (the GUI) can show progress without parsing
/// stdout. The CLI passes `None` and relies on its `println!`s.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// A notice (recovered tool calls, hitting the cap, a continuation, finalization).
    Info(String),
    /// The model's reasoning channel for a step (if the model emits one).
    Thinking(String),
    /// A tool the model invoked, rendered as `name(args)`.
    ToolCall(String),
    /// A tool's (truncated) result preview.
    ToolResult(String),
    /// Per-call performance line (tok/s, load, total).
    Perf(String),
    /// Per-turn hardware line (GPU/CPU split from /api/ps).
    Hardware(String),
    /// The final assistant answer for the turn.
    Final(String),
}

fn send_ev(emit: Option<&Sender<AgentEvent>>, ev: AgentEvent) {
    if let Some(tx) = emit {
        let _ = tx.send(ev);
    }
}

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

/// Human-readable performance line from one model call's telemetry: generation and prompt
/// throughput (tok/s), model load time, and wall time. This is the "how did it run" signal.
fn perf_line(resp: &ChatResponse) -> String {
    let per_s = |count: Option<u64>, dur_ns: Option<u64>| -> f64 {
        match (count, dur_ns) {
            (Some(c), Some(d)) if d > 0 => c as f64 / (d as f64 / 1e9),
            _ => 0.0,
        }
    };
    let gen_tps = per_s(resp.eval_count, resp.eval_duration);
    let prompt_tps = per_s(resp.prompt_eval_count, resp.prompt_eval_duration);
    let load_ms = resp.load_duration.unwrap_or(0) as f64 / 1e6;
    let total_s = resp.total_duration.unwrap_or(0) as f64 / 1e9;
    format!(
        "perf: {gen_tps:.1} tok/s gen ({} tok) · {prompt_tps:.0} tok/s prompt ({} tok) · load {load_ms:.0}ms · total {total_s:.1}s",
        resp.eval_count.unwrap_or(0),
        resp.prompt_eval_count.unwrap_or(0),
    )
}

/// Human-readable hardware line from Ollama's `/api/ps`: for the active model, how much is
/// resident in VRAM vs total — i.e. the GPU/CPU split. Answers "did it run on the GPU?".
fn hardware_line(ps: &Value, model: &str) -> String {
    let gb = |b: u64| b as f64 / 1e9;
    if let Some(models) = ps.get("models").and_then(|m| m.as_array()) {
        for m in models {
            let name = m
                .get("name")
                .or_else(|| m.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if model.is_empty() || name == model {
                let size = m.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                let vram = m.get("size_vram").and_then(|v| v.as_u64()).unwrap_or(0);
                let split = if size == 0 {
                    "unknown".to_string()
                } else if vram == 0 {
                    "100% CPU".to_string()
                } else if vram >= size {
                    "100% GPU".to_string()
                } else {
                    let g = (vram as f64 / size as f64) * 100.0;
                    format!("{g:.0}% GPU / {:.0}% CPU", 100.0 - g)
                };
                return format!(
                    "hardware: {name} — {:.1}GB total, {:.1}GB in VRAM → {split} (ctx {})",
                    gb(size),
                    gb(vram),
                    m.get("context_length").and_then(|v| v.as_u64()).unwrap_or(0)
                );
            }
        }
    }
    "hardware: (no model resident — /api/ps returned nothing for it)".to_string()
}

/// Log per-call perf to the debug log (always), echo on screen when verbose, emit to the UI.
fn log_perf(resp: &ChatResponse, cfg: &Config, emit: Option<&Sender<AgentEvent>>) {
    let line = perf_line(resp);
    debug::log(&format!("  {line}"));
    if cfg.verbose {
        println!("    ⚙ {line}");
    }
    send_ev(emit, AgentEvent::Perf(line));
}

/// Log the hardware split once per turn (best-effort): debug log always, screen when verbose,
/// and emit to the UI.
async fn log_hardware(client: &OllamaClient, cfg: &Config, emit: Option<&Sender<AgentEvent>>) {
    if let Some(ps) = client.ps().await {
        let line = hardware_line(&ps, client.model());
        debug::log(&format!("  {line}"));
        if cfg.verbose {
            println!("    ⚙ {line}");
        }
        send_ev(emit, AgentEvent::Hardware(line));
    }
}

/// Convenience wrapper for the CLI, which doesn't stream events.
pub async fn run_turn(
    client: &OllamaClient,
    router: &ToolRouter,
    ctx: &ToolContext,
    cfg: &Config,
    messages: &mut Vec<ChatMessage>,
) -> String {
    run_turn_streaming(client, router, ctx, cfg, messages, None).await
}

/// `_run_turn`: model + tool loop with iteration cap and optional continuations. `emit`, when
/// present, receives `AgentEvent`s so a UI can show live progress.
pub async fn run_turn_streaming(
    client: &OllamaClient,
    router: &ToolRouter,
    ctx: &ToolContext,
    cfg: &Config,
    messages: &mut Vec<ChatMessage>,
    emit: Option<&Sender<AgentEvent>>,
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
        if let Some(t) = &resp.message.thinking {
            send_ev(emit, AgentEvent::Thinking(t.clone()));
        }
        log_perf(&resp, cfg, emit);
        log_hardware(client, cfg, emit).await; // once per turn — model is now resident
        let (mut tcs, mut recovered) = get_tool_calls(&resp);
        let mut last_resp = resp;

        let mut iters = 0usize;
        while !tcs.is_empty() && iters < cfg.max_tool_iters {
            iters += 1;
            messages.push(last_resp.message.clone());
            if recovered {
                let msg = format!(
                    "recovered {} tool-call(s) from text — model lacks native tool_calls",
                    tcs.len()
                );
                println!("  [{msg}]");
                debug::log(&msg);
                send_ev(emit, AgentEvent::Info(msg));
            }
            for tc in &tcs {
                let name = &tc.function.name;
                let args = &tc.function.arguments;
                let call = format!("{}({})", name, shown(args));
                println!("  → {call}");
                send_ev(emit, AgentEvent::ToolCall(call));
                debug::audit(&cfg.log_file, name, args);
                debug::log(&format!("tool-call #{iters} {name} args={args}"));
                let t0 = Instant::now();
                let result = run_tool(router, ctx, name, args.clone()).await;
                tool_count += 1;
                if cfg.verbose {
                    println!("    ⏱  {name} took {:.1}s", t0.elapsed().as_secs_f64());
                }
                debug::log(&format!("tool-result {name} -> {}", truncate_chars(&result, 2000)));
                send_ev(emit, AgentEvent::ToolResult(format!("{name}: {}", truncate_chars(&result, 400))));
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
            if let Some(t) = &resp.message.thinking {
                send_ev(emit, AgentEvent::Thinking(t.clone()));
            }
            log_perf(&resp, cfg, emit);
            let g = get_tool_calls(&resp);
            tcs = g.0;
            recovered = g.1;
            last_resp = resp;
        }

        // Hit the cap but the model still wants tools -> bounded continuation.
        if !tcs.is_empty() && continuations < cfg.max_continuations {
            continuations += 1;
            let msg = format!(
                "hit {}-tool cap — continuation {}/{}",
                cfg.max_tool_iters, continuations, cfg.max_continuations
            );
            println!("  [{msg}]");
            send_ev(emit, AgentEvent::Info(msg));
            messages.push(last_resp.message.clone());
            for tc in &tcs {
                let name = &tc.function.name;
                let args = &tc.function.arguments;
                let call = format!("{}({})", name, shown(args));
                println!("  → {call}");
                send_ev(emit, AgentEvent::ToolCall(call));
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
                let msg = "tool limit reached — asking the model to write up its findings";
                println!("  [{msg}]");
                send_ev(emit, AgentEvent::Info(msg.to_string()));
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
        // The final answer is delivered as the return value (the CLI prints it; the GUI thread
        // wraps it in AgentEvent::Final so error-path early returns are covered too).
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
