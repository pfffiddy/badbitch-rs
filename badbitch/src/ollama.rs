//! Ollama `/api/chat` client + our own message types — the AI-layer swap that replaces
//! sfull's Anthropic SDK. We keep `content` as a raw `String` so the tool-call recovery in
//! `recovery_calls.rs` can scan text the model leaked instead of populating `tool_calls`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Config;
use crate::tool::ToolSpec;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Some models emit a separate reasoning channel; kept for the debug log, never sent back.
    #[serde(default, skip_serializing)]
    pub thinking: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::simple("system", content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::simple("user", content)
    }
    pub fn simple(role: &str, content: impl Into<String>) -> Self {
        ChatMessage {
            role: role.to_string(),
            content: content.into(),
            tool_calls: None,
            name: None,
            thinking: None,
        }
    }
    pub fn tool_result(name: &str, content: impl Into<String>) -> Self {
        ChatMessage {
            role: "tool".to_string(),
            content: content.into(),
            tool_calls: None,
            name: Some(name.to_string()),
            thinking: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub message: ChatMessage,
    #[serde(default)]
    pub done_reason: Option<String>,
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<u64>,
    #[serde(default)]
    pub total_duration: Option<u64>,
    #[serde(default)]
    pub load_duration: Option<u64>,
}

pub struct OllamaClient {
    http: reqwest::Client,
    host: String,
    model: String,
    options: Value,
}

impl OllamaClient {
    pub fn new(cfg: &Config) -> Self {
        OllamaClient {
            http: reqwest::Client::new(),
            host: cfg.ollama_host.clone(),
            model: cfg.model.clone(),
            // `_gen_options` (badbitch2.py:1639)
            options: json!({
                "num_ctx": cfg.num_ctx,
                "temperature": cfg.gen_temp,
                "top_p": cfg.gen_top_p,
                "repeat_penalty": cfg.gen_repeat,
            }),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Tool-free model call used by the TL;DR summarizer (`_summarize_turn`, badbitch2.py:1601).
    /// Uses lower temperature for a more deterministic summary.
    pub async fn chat_no_tools(
        &self,
        messages: &[ChatMessage],
        cfg: &Config,
    ) -> anyhow::Result<ChatResponse> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
            "options": {
                "num_ctx": cfg.num_ctx,
                "temperature": 0.2,
                "top_p": cfg.gen_top_p,
                "repeat_penalty": cfg.gen_repeat,
            },
        });
        let resp = self
            .http
            .post(format!("{}/api/chat", self.host))
            .timeout(Duration::from_secs(120))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("ollama HTTP {}: {}", status.as_u16(), text);
        }
        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("ollama: bad response: {e}; body={}", &text))?;
        Ok(parsed)
    }

    /// One `ollama.chat` call (badbitch2.py:1653) with tools + sampling options.
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> anyhow::Result<ChatResponse> {
        let wire_tools: Vec<Value> = tools.iter().map(tool_to_wire).collect();
        let body = json!({
            "model": self.model,
            "messages": messages,
            "tools": wire_tools,
            "stream": false,
            "options": self.options,
        });
        let resp = self
            .http
            .post(format!("{}/api/chat", self.host))
            .timeout(Duration::from_secs(600))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("ollama HTTP {}: {}", status.as_u16(), text);
        }
        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("ollama: bad response: {e}; body={}", &text))?;
        Ok(parsed)
    }
}

fn tool_to_wire(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description.clone().unwrap_or_default(),
            "parameters": spec.input_schema,
        }
    })
}
