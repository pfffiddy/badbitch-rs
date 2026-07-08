//! The contract between the controller (authoritative, lives with Ollama on
//! the desktop) and any front-end (the desktop window, or a phone over TLS).
//!
//! Front-ends are dumb: they render an `AppState` snapshot and emit `Cmd`s.
//! Because the desktop UI itself goes through this layer, the phone gets the
//! exact same feature set for free.

use crate::autotune::{ParamOverrides, SystemInfo};
use crate::backend::ModelInfo;
use crate::tools::ToolDef;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Cheap "wake up, something changed" callback (e.g. egui request_repaint).
pub type Notify = Arc<dyn Fn() + Send + Sync>;

// ---------------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptItem {
    User { text: String },
    AssistantStreaming { text: String },
    Assistant { thought: String, text: String },
    ToolCall { tool: String, args: String, thought: String },
    ToolResult { tool: String, output: String },
    Info { text: String },
    Error { text: String },
}

// ---------------------------------------------------------------------------
// Commands (client → controller)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Cmd {
    SendPrompt { text: String },
    Stop,
    ClearConversation,

    SelectModel { name: String },
    RefreshModels,
    RedetectSystem,

    SetSystemPrompt { text: String },
    SetMaxSteps { steps: usize },
    SetParams { params: ParamOverrides },

    AddTool { tool: ToolDef },
    RemoveTool { name: String },
    SetToolEnabled { name: String, enabled: bool },

    Pull { name: String },
    ImportPath { path: String },
    CancelImport,
    SetLlamaCppDir { dir: String },
    SetQuantize { quantize: String },

    /// Progress of a model file streaming in from a phone. The desktop's own
    /// remote server emits this as bytes arrive so every client sees the
    /// upload advancing; it is not a user-triggered action.
    UploadStatus { received: u64, total: u64 },

    // Remote-access administration (applies to the desktop's server; a phone
    // may send these too — you can manage the server from an already-paired
    // device).
    SetRemoteEnabled { enabled: bool },
    SetRemotePort { port: u16 },
    StartPairing,
    CancelPairing,
    RevokeDevice { id: String },
}

// ---------------------------------------------------------------------------
// State snapshot (controller → clients)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestView {
    pub running: bool,
    pub progress: Option<f32>,
    pub log: Vec<String>,
    pub llama_cpp_dir: String,
    pub quantize: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub created_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingView {
    pub code: String,
    pub secs_left: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteView {
    pub enabled: bool,
    pub port: u16,
    /// SHA-256 of the server certificate (hex) — what clients pin.
    pub fingerprint: String,
    /// Local addresses a phone can try (best-effort detection).
    pub addresses: Vec<String>,
    pub pairing: Option<PairingView>,
    pub devices: Vec<DeviceView>,
    pub connected_clients: usize,
    pub last_error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    pub version: u64,
    pub status: String,

    pub models: Vec<ModelInfo>,
    pub selected_model: String,

    pub sys: SystemInfo,
    pub params: ParamOverrides,
    pub system_prompt: String,
    pub max_steps: usize,

    pub tools: Vec<ToolDef>,

    pub transcript: Vec<TranscriptItem>,
    pub agent_running: bool,

    pub ingest: IngestView,
    pub remote: RemoteView,
}

// ---------------------------------------------------------------------------
// Wire messages (WebSocket text frames, JSON)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First message on a pairing connection. `mac` = hex
    /// HMAC-SHA256(key = normalized pairing code,
    ///             msg = server cert fingerprint || device name) —
    /// proves knowledge of the code without revealing it, and binds it to
    /// the certificate the client actually saw (foils a MITM's cert swap).
    Pair { name: String, mac: String },
    /// First message on a normal connection: the device token from pairing.
    Auth { token: String },
    Cmd { cmd: Cmd },

    /// Begin streaming a model file up to the desktop. The file's bytes follow
    /// as WebSocket *binary* frames; `UploadEnd` closes the stream and makes
    /// the desktop import the received file (same pipeline as a local drop).
    /// `name` is the suggested filename (must end in `.gguf`); `size` is the
    /// total byte count, for progress.
    UploadBegin { name: String, size: u64 },
    /// All bytes sent — import the received file.
    UploadEnd,
    /// Abort the in-progress upload and discard the partial file.
    UploadCancel,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    PairOk { token: String, fp: String },
    AuthOk,
    Err { msg: String },
    State { state: AppState },
}

// ---------------------------------------------------------------------------
// Shared, versioned state cell (controller writes; clients read/wait)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct SharedState(Arc<(Mutex<AppState>, Condvar)>);

impl SharedState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> AppState {
        self.0 .0.lock().unwrap().clone()
    }

    pub fn version(&self) -> u64 {
        self.0 .0.lock().unwrap().version
    }

    /// Replace the snapshot (bumping the version) and wake all waiters.
    pub fn publish(&self, mut state: AppState) {
        let mut guard = self.0 .0.lock().unwrap();
        state.version = guard.version + 1;
        *guard = state;
        self.0 .1.notify_all();
    }

    /// Store a snapshot keeping its existing version (used by the remote
    /// client, which mirrors the server's counter).
    pub fn publish_raw(&self, state: AppState) {
        let mut guard = self.0 .0.lock().unwrap();
        *guard = state;
        self.0 .1.notify_all();
    }

    /// Block up to `timeout` for a version newer than `seen`.
    pub fn wait_newer(&self, seen: u64, timeout: Duration) -> Option<AppState> {
        let (lock, cv) = (&self.0 .0, &self.0 .1);
        let guard = lock.lock().unwrap();
        let (guard, _) = cv
            .wait_timeout_while(guard, timeout, |s| s.version <= seen)
            .unwrap();
        (guard.version > seen).then(|| guard.clone())
    }
}
