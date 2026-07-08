//! Backend abstraction + the Ollama implementation.
//!
//! Everything the UI/agent needs from an inference engine goes through the
//! `Backend` trait. To add another backend later (e.g. an in-process GGUF
//! engine via `llama-cpp-2`), implement this trait and hand the app an
//! `Arc<dyn Backend>` — nothing else changes.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

/// One chat message. Roles: "system" | "user" | "assistant" | "tool".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self { role: role.to_string(), content: content.into() }
    }
}

/// A fully-resolved chat request. `options` holds engine parameters
/// (num_gpu, num_ctx, temperature, ...) as a JSON object; `format` is an
/// optional JSON Schema — when present the backend must constrain decoding
/// so the output validates against it (Ollama structured outputs).
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub options: serde_json::Value,
    pub format: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub size_bytes: u64,
    /// e.g. "7.6B" — used by the auto-tuner to estimate layer count
    pub parameter_size: String,
    pub quantization: String,
}

/// Progress callback contract: return `false` to cancel the operation.
pub type TokenSink<'a> = &'a mut dyn FnMut(&str) -> bool;
pub type ProgressSink<'a> = &'a mut dyn FnMut(&str, Option<f32>) -> bool;

pub trait Backend: Send + Sync {
    #[allow(dead_code)] // used by future backends / diagnostics
    fn name(&self) -> &'static str;

    /// Make sure the engine is reachable, starting it if necessary.
    /// Returns true if this call started the server.
    fn ensure_running(&self) -> Result<bool>;

    fn list_models(&self) -> Result<Vec<ModelInfo>>;

    /// Stream a chat completion. `on_token` receives each content chunk and
    /// may return `false` to abort mid-generation. Returns the full text.
    fn chat_stream(&self, req: &ChatRequest, on_token: TokenSink) -> Result<String>;

    /// Pull a model by name from the backend's registry.
    fn pull_model(&self, name: &str, progress: ProgressSink) -> Result<()>;

    /// Import a local GGUF file as a named model.
    fn import_gguf(&self, gguf_path: &str, model_name: &str, progress: ProgressSink) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Ollama implementation
// ---------------------------------------------------------------------------

pub struct OllamaBackend {
    pub base_url: String,
}

impl OllamaBackend {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self { base_url: base_url.into() }
    }

    fn ping(&self) -> bool {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(800))
            .build()
            .get(&format!("{}/api/tags", self.base_url))
            .call()
            .is_ok()
    }
}

impl Backend for OllamaBackend {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn ensure_running(&self) -> Result<bool> {
        if self.ping() {
            return Ok(false);
        }
        // Not reachable — try to start `ollama serve` detached.
        Command::new("ollama")
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn `ollama serve` — is Ollama installed and on PATH?")?;
        // Poll until the HTTP API answers (up to ~15 s).
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(250));
            if self.ping() {
                return Ok(true);
            }
        }
        bail!("started `ollama serve` but {} never became reachable", self.base_url)
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let resp: serde_json::Value = ureq::get(&format!("{}/api/tags", self.base_url))
            .call()
            .context("GET /api/tags failed")?
            .into_json()?;
        let mut out = Vec::new();
        if let Some(models) = resp["models"].as_array() {
            for m in models {
                out.push(ModelInfo {
                    name: m["name"].as_str().unwrap_or_default().to_string(),
                    size_bytes: m["size"].as_u64().unwrap_or(0),
                    parameter_size: m["details"]["parameter_size"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    quantization: m["details"]["quantization_level"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }
        Ok(out)
    }

    fn chat_stream(&self, req: &ChatRequest, on_token: TokenSink) -> Result<String> {
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "stream": true,
            "options": req.options,
        });
        if let Some(fmt) = &req.format {
            // Ollama ≥0.5: `format` accepts a full JSON Schema and constrains
            // decoding (grammar-level enforcement), not just a hint.
            body["format"] = fmt.clone();
        }

        let resp = ureq::post(&format!("{}/api/chat", self.base_url))
            .send_json(body)
            .map_err(|e| anyhow!("POST /api/chat failed: {e}"))?;

        let reader = BufReader::new(resp.into_reader());
        let mut full = String::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(&line)
                .with_context(|| format!("bad NDJSON line from ollama: {line}"))?;
            if let Some(err) = v["error"].as_str() {
                bail!("ollama error: {err}");
            }
            if let Some(chunk) = v["message"]["content"].as_str() {
                if !chunk.is_empty() {
                    full.push_str(chunk);
                    if !on_token(chunk) {
                        // Dropping the reader closes the connection → Ollama
                        // stops generating. Clean mid-stream cancellation.
                        return Ok(full);
                    }
                }
            }
            if v["done"].as_bool() == Some(true) {
                break;
            }
        }
        Ok(full)
    }

    fn pull_model(&self, name: &str, progress: ProgressSink) -> Result<()> {
        let resp = ureq::post(&format!("{}/api/pull", self.base_url))
            .send_json(serde_json::json!({ "name": name, "stream": true }))
            .map_err(|e| anyhow!("POST /api/pull failed: {e}"))?;
        let reader = BufReader::new(resp.into_reader());
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(&line)?;
            if let Some(err) = v["error"].as_str() {
                bail!("pull failed: {err}");
            }
            let status = v["status"].as_str().unwrap_or("");
            let frac = match (v["completed"].as_f64(), v["total"].as_f64()) {
                (Some(c), Some(t)) if t > 0.0 => Some((c / t) as f32),
                _ => None,
            };
            if !progress(status, frac) {
                bail!("pull cancelled");
            }
        }
        Ok(())
    }

    fn import_gguf(&self, gguf_path: &str, model_name: &str, progress: ProgressSink) -> Result<()> {
        // Generate a Modelfile and shell out to `ollama create`, which handles
        // hashing/uploading the blob into Ollama's store.
        let dir = std::env::temp_dir().join(format!("llm-desk-import-{model_name}"));
        std::fs::create_dir_all(&dir)?;
        let modelfile = dir.join("Modelfile");
        std::fs::write(&modelfile, format!("FROM {gguf_path}\n"))?;
        progress(&format!("Modelfile written: FROM {gguf_path}"), None);

        let mut child = Command::new("ollama")
            .args(["create", model_name, "-f"])
            .arg(&modelfile)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to run `ollama create`")?;

        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if !progress(&line, None) {
                    let _ = child.kill();
                    bail!("import cancelled");
                }
            }
        }
        let status = child.wait()?;
        if !status.success() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut err);
            }
            bail!("`ollama create` failed: {err}");
        }
        progress(&format!("model '{model_name}' created"), None);
        Ok(())
    }
}
