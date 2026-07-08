//! The agentic loop.
//!
//! Runs on its own thread; the UI owns the conversation in an
//! `Arc<Mutex<Vec<ChatMessage>>>` so state survives cancellation. Progress is
//! reported over an mpsc channel. Cancellation is an `Arc<AtomicBool>`
//! checked (a) between every step and (b) on every streamed token, so Stop
//! interrupts both between tool calls and mid-generation, leaving the
//! transcript intact for a resumed instruction.

use crate::backend::{Backend, ChatMessage, ChatRequest};
use crate::protocol::Notify;
use crate::tools::{build_tool_from_args, response_schema, system_prompt_block, ToolDef, ToolKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub enum AgentEvent {
    /// A streamed chunk of the assistant's (JSON) output.
    Token(String),
    /// The model decided to call a tool.
    ToolCall { tool: String, args: serde_json::Value, thought: String },
    /// A tool finished executing.
    ToolResult { tool: String, output: String },
    /// The model produced its final answer.
    FinalAnswer { text: String, thought: String },
    /// The model created a new tool via `create_tool`; it is already in the
    /// shared registry — the UI should persist it and show it.
    ToolCreated(ToolDef),
    Status(String),
    Error(String),
    /// Loop ended (finished, cancelled, or errored). `cancelled` tells which.
    Done { cancelled: bool },
}

pub struct AgentHandle {
    pub cancel: Arc<AtomicBool>,
    pub events: Receiver<AgentEvent>,
}

pub struct AgentConfig {
    pub model: String,
    pub options: serde_json::Value,
    pub system_prompt: String,
    /// Shared, live registry: `create_tool` pushes into it mid-run, and the
    /// per-step schema regeneration below makes new tools callable at once.
    pub tools: std::sync::Arc<Mutex<Vec<ToolDef>>>,
    pub max_steps: usize,
}

pub fn spawn_agent(
    backend: Arc<dyn Backend>,
    cfg: AgentConfig,
    conversation: Arc<Mutex<Vec<ChatMessage>>>,
    notify: Notify, // called after every event so UIs can repaint
) -> AgentHandle {
    let (tx, rx) = std::sync::mpsc::channel::<AgentEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel2 = cancel.clone();

    std::thread::spawn(move || {
        run_loop(backend, cfg, conversation, cancel2, tx.clone(), notify.clone());
        notify();
    });

    AgentHandle { cancel, events: rx }
}

fn send(tx: &Sender<AgentEvent>, notify: &Notify, ev: AgentEvent) {
    let _ = tx.send(ev);
    notify();
}

fn run_loop(
    backend: Arc<dyn Backend>,
    cfg: AgentConfig,
    conversation: Arc<Mutex<Vec<ChatMessage>>>,
    cancel: Arc<AtomicBool>,
    tx: Sender<AgentEvent>,
    notify: Notify,
) {
    for _step in 0..cfg.max_steps {
        // Snapshot the registry and regenerate the contract every step, so a
        // tool created on the previous step is immediately usable.
        let tools: Vec<ToolDef> = cfg.tools.lock().unwrap().clone();
        let tools_enabled = tools.iter().any(|t| t.enabled);
        let schema = tools_enabled.then(|| response_schema(&tools));
        let system = format!("{}{}", cfg.system_prompt, system_prompt_block(&tools));

        // --- cancellation checkpoint: between steps ---
        if cancel.load(Ordering::Relaxed) {
            send(&tx, &notify, AgentEvent::Done { cancelled: true });
            return;
        }

        // Snapshot conversation, prepend the (regenerated) system message.
        let mut messages = vec![ChatMessage::new("system", system.clone())];
        messages.extend(conversation.lock().unwrap().iter().cloned());

        let req = ChatRequest {
            model: cfg.model.clone(),
            messages,
            options: cfg.options.clone(),
            format: schema,
        };

        // --- stream one model turn; cancellation checkpoint: every token ---
        let tx_tok = tx.clone();
        let notify_tok = notify.clone();
        let cancel_tok = cancel.clone();
        let result = backend.chat_stream(&req, &mut |chunk| {
            send(&tx_tok, &notify_tok, AgentEvent::Token(chunk.to_string()));
            !cancel_tok.load(Ordering::Relaxed)
        });

        let content = match result {
            Ok(c) => c,
            Err(e) => {
                send(&tx, &notify, AgentEvent::Error(format!("inference failed: {e}")));
                send(&tx, &notify, AgentEvent::Done { cancelled: false });
                return;
            }
        };

        // Persist the assistant turn even if we're about to stop — the user
        // can resume with full context.
        conversation.lock().unwrap().push(ChatMessage::new("assistant", content.clone()));

        if cancel.load(Ordering::Relaxed) {
            send(&tx, &notify, AgentEvent::Done { cancelled: true });
            return;
        }

        // --- interpret the turn ---
        if !tools_enabled {
            send(&tx, &notify, AgentEvent::FinalAnswer { text: content, thought: String::new() });
            send(&tx, &notify, AgentEvent::Done { cancelled: false });
            return;
        }

        let parsed: serde_json::Value = match serde_json::from_str(content.trim()) {
            Ok(v) => v,
            Err(_) => {
                // Grammar constraint should prevent this; treat as final answer.
                send(&tx, &notify, AgentEvent::FinalAnswer { text: content, thought: String::new() });
                send(&tx, &notify, AgentEvent::Done { cancelled: false });
                return;
            }
        };
        let thought = parsed["thought"].as_str().unwrap_or("").to_string();

        match parsed["action"].as_str() {
            Some("tool_call") => {
                let tool_name = parsed["tool"].as_str().unwrap_or("").to_string();
                let args = parsed["arguments"].clone();
                send(
                    &tx,
                    &notify,
                    AgentEvent::ToolCall { tool: tool_name.clone(), args: args.clone(), thought },
                );

                let output = match tools.iter().find(|t| t.enabled && t.name == tool_name) {
                    Some(tool) if tool.kind == ToolKind::CreateTool => {
                        // Handled app-side: validate, add to the live registry.
                        let mut reg = cfg.tools.lock().unwrap();
                        match build_tool_from_args(&args, &reg) {
                            Ok(new_tool) => {
                                let summary = format!(
                                    "created tool '{}' — it is enabled and available from your next step. \
                                     Command template: {}",
                                    new_tool.name, new_tool.command
                                );
                                reg.push(new_tool.clone());
                                drop(reg);
                                send(&tx, &notify, AgentEvent::ToolCreated(new_tool));
                                summary
                            }
                            Err(e) => e,
                        }
                    }
                    Some(tool) => tool.execute(&args),
                    None => format!("error: unknown tool '{tool_name}'"),
                };
                send(
                    &tx,
                    &notify,
                    AgentEvent::ToolResult { tool: tool_name.clone(), output: output.clone() },
                );
                conversation.lock().unwrap().push(ChatMessage::new(
                    "tool",
                    format!("Result of {tool_name}:\n{output}"),
                ));
                // loop continues → model reads the result and decides next step
            }
            _ => {
                let text = parsed["final_answer"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| content.clone());
                send(&tx, &notify, AgentEvent::FinalAnswer { text, thought });
                send(&tx, &notify, AgentEvent::Done { cancelled: false });
                return;
            }
        }
    }

    send(&tx, &notify, AgentEvent::Status("max steps reached".into()));
    send(&tx, &notify, AgentEvent::Done { cancelled: false });
}
