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
    pub prompt_eval_duration: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<u64>,
    #[serde(default)]
    pub eval_duration: Option<u64>,
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
    think: Option<bool>,
}

impl OllamaClient {
    pub fn new(cfg: &Config) -> Self {
        // Base sampling options, then merge any [model_options] pass-through (which can
        // override the base and add anything `ollama run` supports: top_k, num_gpu, mirostat…).
        let mut opts = serde_json::Map::new();
        opts.insert("num_ctx".into(), json!(cfg.num_ctx));
        opts.insert("temperature".into(), json!(cfg.gen_temp));
        opts.insert("top_p".into(), json!(cfg.gen_top_p));
        opts.insert("repeat_penalty".into(), json!(cfg.gen_repeat));
        for (k, v) in &cfg.model_options {
            opts.insert(k.clone(), v.clone());
        }
        OllamaClient {
            http: reqwest::Client::new(),
            host: cfg.ollama_host.clone(),
            model: cfg.model.clone(),
            options: Value::Object(opts),
            think: cfg.think,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Query Ollama's `/api/ps` — the loaded models and how much of each is resident in VRAM.
    /// Used to log "how the hardware handled it" (GPU vs CPU split). Best-effort; None on error.
    pub async fn ps(&self) -> Option<Value> {
        let resp = self
            .http
            .get(format!("{}/api/ps", self.host))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    }

    /// Tool-free model call — used by the TL;DR summarizer and the forced-finalization
    /// wrap-up. `temperature` lets the caller pick (low for a tight summary, the normal
    /// generation temp for a full write-up). Uses the same generous timeout as the main
    /// tool loop: the finalization pass writes a FULL dossier, which on a large model can
    /// take minutes — a short timeout here silently drops the write-up and the summary,
    /// leaving the user with "[no content]" and no TL;DR.
    pub async fn chat_no_tools(
        &self,
        messages: &[ChatMessage],
        cfg: &Config,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
            "options": {
                "num_ctx": cfg.num_ctx,
                "temperature": temperature,
                "top_p": cfg.gen_top_p,
                "repeat_penalty": cfg.gen_repeat,
            },
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

    /// One `ollama.chat` call (badbitch2.py:1653) with tools + sampling options.
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> anyhow::Result<ChatResponse> {
        let wire_tools: Vec<Value> = tools.iter().map(tool_to_wire).collect();
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "tools": wire_tools,
            "stream": false,
            "options": self.options,
        });
        // Enable/disable a reasoning model's thinking channel when configured.
        if let Some(t) = self.think {
            body["think"] = json!(t);
        }
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

/// List locally-installed Ollama model names via `/api/tags` (for the GUI's model picker).
/// Best-effort — returns an empty list if Ollama is unreachable.
pub async fn list_models(host: &str) -> Vec<String> {
    let url = format!("{}/api/tags", host.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let resp = match client.get(url).timeout(Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    let v: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    v.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
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
