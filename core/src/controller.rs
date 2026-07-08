//! The controller owns everything the old single-window app owned: the
//! backend, the conversation, the tool registry, agent/ingest handles, and
//! the remote-access server. It runs on its own thread, applies `Cmd`s from
//! any number of clients, and publishes `AppState` snapshots.
//!
//! The desktop window and a paired phone are both just clients of this.

use crate::agent::{spawn_agent, AgentConfig, AgentEvent, AgentHandle};
use crate::autotune::{ParamOverrides, SystemInfo};
use crate::backend::{Backend, ChatMessage, ModelInfo};
use crate::ingest::{classify, spawn_ingest, spawn_pull, IngestEvent, IngestHandle, IngestJob, ModelSource};
use crate::protocol::*;
use crate::remote::pairing::{self, RemoteStore};
use crate::remote::server::{self, ServerCtl};
use crate::tools::{self, save_tools, ToolDef};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct ControllerHandle {
    pub cmds: Sender<Cmd>,
    pub shared: SharedState,
    notifiers: Arc<Mutex<Vec<Notify>>>,
}

impl ControllerHandle {
    pub fn add_notifier(&self, n: Notify) {
        self.notifiers.lock().unwrap().push(n);
    }
    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }
}

enum Misc {
    Status(String),
    Models(Vec<ModelInfo>),
}

struct Controller {
    backend: Arc<dyn Backend>,
    misc_tx: Sender<Misc>,
    misc_rx: Receiver<Misc>,

    status: String,
    sys: SystemInfo,
    models: Vec<ModelInfo>,
    selected_model: String,
    params: ParamOverrides,
    system_prompt: String,
    max_steps: usize,

    tools: Arc<Mutex<Vec<ToolDef>>>,
    conversation: Arc<Mutex<Vec<ChatMessage>>>,
    transcript: Vec<TranscriptItem>,
    agent: Option<AgentHandle>,

    ingest: Option<IngestHandle>,
    ingest_log: Vec<String>,
    ingest_progress: Option<f32>,
    llama_cpp_dir: String,
    quantize: String,

    // remote access
    remote_port: u16,
    remote_store: Arc<Mutex<RemoteStore>>,
    remote_clients: Arc<AtomicUsize>,
    remote_server: Option<ServerCtl>,
    remote_error: String,

    shared: SharedState,
    #[allow(dead_code)] // kept alive so late-registered notifiers still fire
    notifiers: Arc<Mutex<Vec<Notify>>>,
    notify: Notify,
    cmds_tx: Sender<Cmd>, // handed to the remote server so its clients feed us
}

pub fn spawn(backend: Arc<dyn Backend>) -> ControllerHandle {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    let (misc_tx, misc_rx) = std::sync::mpsc::channel::<Misc>();
    let shared = SharedState::new();
    let notifiers: Arc<Mutex<Vec<Notify>>> = Arc::new(Mutex::new(Vec::new()));

    let handle = ControllerHandle {
        cmds: cmd_tx.clone(),
        shared: shared.clone(),
        notifiers: notifiers.clone(),
    };

    let notify: Notify = {
        let notifiers = notifiers.clone();
        Arc::new(move || {
            for n in notifiers.lock().unwrap().iter() {
                n();
            }
        })
    };

    std::thread::spawn(move || {
        let mut c = Controller {
            backend,
            misc_tx,
            misc_rx,
            status: "starting…".into(),
            sys: SystemInfo::detect(),
            models: vec![],
            selected_model: String::new(),
            params: ParamOverrides::default(),
            system_prompt: "You are a helpful assistant running locally on the user's machine."
                .into(),
            max_steps: 16,
            tools: Arc::new(Mutex::new(tools::load_tools())),
            conversation: Arc::new(Mutex::new(Vec::new())),
            transcript: vec![],
            agent: None,
            ingest: None,
            ingest_log: vec![],
            ingest_progress: None,
            llama_cpp_dir: format!(
                "{}/llama.cpp",
                std::env::var("HOME").unwrap_or_else(|_| ".".into())
            ),
            quantize: "Q4_K_M".into(),
            remote_port: 4832,
            remote_store: Arc::new(Mutex::new(pairing::load_store(&tools::config_dir()))),
            remote_clients: Arc::new(AtomicUsize::new(0)),
            remote_server: None,
            remote_error: String::new(),
            shared,
            notifiers,
            notify,
            cmds_tx: cmd_tx,
        };
        c.startup();
        c.run(cmd_rx);
    });

    handle
}

impl Controller {
    fn startup(&mut self) {
        // Ensure Ollama is up + fetch model list, off-thread.
        let backend = self.backend.clone();
        let tx = self.misc_tx.clone();
        let notify = self.notify.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Misc::Status("checking Ollama…".into()));
            notify();
            match backend.ensure_running() {
                Ok(started) => {
                    let _ = tx.send(Misc::Status(if started {
                        "Ollama was not running — started `ollama serve`".into()
                    } else {
                        "connected to Ollama".into()
                    }));
                    match backend.list_models() {
                        Ok(m) => { let _ = tx.send(Misc::Models(m)); }
                        Err(e) => { let _ = tx.send(Misc::Status(format!("list models failed: {e}"))); }
                    }
                }
                Err(e) => { let _ = tx.send(Misc::Status(format!("⚠ {e}"))); }
            }
            notify();
        });
    }

    fn run(&mut self, cmd_rx: Receiver<Cmd>) {
        loop {
            // Pace the loop on the command channel; 25 ms keeps token
            // streaming feeling live without busy-spinning.
            match cmd_rx.recv_timeout(Duration::from_millis(25)) {
                Ok(cmd) => self.apply(cmd),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
            // Drain any further queued commands this tick.
            while let Ok(cmd) = cmd_rx.try_recv() {
                self.apply(cmd);
            }
            while let Ok(m) = self.misc_rx.try_recv() {
                match m {
                    Misc::Status(s) => self.status = s,
                    Misc::Models(m) => {
                        if self.selected_model.is_empty() {
                            if let Some(first) = m.first() {
                                self.selected_model = first.name.clone();
                            }
                        }
                        self.models = m;
                    }
                }
            }
            self.drain_agent();
            self.drain_ingest();
            self.publish();
        }
    }

    // ------------------------------------------------------------------
    fn apply(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::SendPrompt { text } => self.start_agent(text),
            Cmd::Stop => {
                if let Some(h) = &self.agent {
                    h.cancel.store(true, Ordering::Relaxed);
                }
            }
            Cmd::ClearConversation => {
                if self.agent.is_none() {
                    self.conversation.lock().unwrap().clear();
                    self.transcript.clear();
                }
            }
            Cmd::SelectModel { name } => {
                // Validate against the known list so a stale client can't set
                // a phantom model, and surface the switch so it's observable.
                if self.models.iter().any(|m| m.name == name) {
                    if self.selected_model != name {
                        self.status = format!("model → {name}");
                    }
                    self.selected_model = name;
                } else {
                    self.status = format!("unknown model '{name}' — refresh the list");
                }
            }
            Cmd::RefreshModels => {
                let backend = self.backend.clone();
                let tx = self.misc_tx.clone();
                let notify = self.notify.clone();
                std::thread::spawn(move || {
                    match backend.list_models() {
                        Ok(m) => { let _ = tx.send(Misc::Models(m)); }
                        Err(e) => { let _ = tx.send(Misc::Status(format!("refresh failed: {e}"))); }
                    }
                    notify();
                });
            }
            Cmd::RedetectSystem => self.sys = SystemInfo::detect(),
            Cmd::SetSystemPrompt { text } => self.system_prompt = text,
            Cmd::SetMaxSteps { steps } => self.max_steps = steps.clamp(1, 64),
            Cmd::SetParams { params } => self.params = params,
            Cmd::AddTool { tool } => {
                let mut t = self.tools.lock().unwrap();
                if !tool.name.trim().is_empty() && !t.iter().any(|x| x.name == tool.name) {
                    t.push(tool);
                    save_tools(&t);
                }
            }
            Cmd::RemoveTool { name } => {
                let mut t = self.tools.lock().unwrap();
                t.retain(|x| x.name != name);
                save_tools(&t);
            }
            Cmd::SetToolEnabled { name, enabled } => {
                let mut t = self.tools.lock().unwrap();
                if let Some(x) = t.iter_mut().find(|x| x.name == name) {
                    x.enabled = enabled;
                }
                save_tools(&t);
            }
            Cmd::Pull { name } => {
                if self.ingest.is_none() && !name.trim().is_empty() {
                    self.ingest_progress = None;
                    self.ingest = Some(spawn_pull(
                        self.backend.clone(),
                        name.trim().to_string(),
                        self.notify.clone(),
                    ));
                }
            }
            Cmd::ImportPath { path } => self.start_ingest_path(path),
            Cmd::UploadStatus { received, total } => {
                let pct = if total > 0 { received * 100 / total } else { 0 };
                self.status = format!(
                    "receiving model upload… {pct}% ({:.0}/{:.0} MB)",
                    received as f64 / 1_048_576.0,
                    total as f64 / 1_048_576.0
                );
            }
            Cmd::CancelImport => {
                if let Some(h) = &self.ingest {
                    h.cancel.store(true, Ordering::Relaxed);
                }
            }
            Cmd::SetLlamaCppDir { dir } => self.llama_cpp_dir = dir,
            Cmd::SetQuantize { quantize } => self.quantize = quantize,

            Cmd::SetRemoteEnabled { enabled } => self.set_remote_enabled(enabled),
            Cmd::SetRemotePort { port } => {
                if self.remote_server.is_none() && port >= 1024 {
                    self.remote_port = port;
                }
            }
            Cmd::StartPairing => {
                if self.remote_server.is_some() {
                    pairing::start_pairing(&self.remote_store);
                }
            }
            Cmd::CancelPairing => {
                self.remote_store.lock().unwrap().pairing = None;
            }
            Cmd::RevokeDevice { id } => {
                let mut store = self.remote_store.lock().unwrap();
                store.devices.retain(|d| d.id != id);
                pairing::save_devices(&tools::config_dir(), &store.devices);
            }
        }
    }

    fn set_remote_enabled(&mut self, enabled: bool) {
        self.remote_error.clear();
        if enabled && self.remote_server.is_none() {
            match server::start(
                self.remote_port,
                &tools::config_dir(),
                self.remote_store.clone(),
                self.remote_clients.clone(),
                self.cmds_tx.clone(),
                self.shared.clone(),
            ) {
                Ok(ctl) => self.remote_server = Some(ctl),
                Err(e) => self.remote_error = format!("could not start server: {e}"),
            }
        } else if !enabled {
            if let Some(ctl) = self.remote_server.take() {
                ctl.stop();
            }
            self.remote_store.lock().unwrap().pairing = None;
        }
    }

    fn start_agent(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() || self.selected_model.is_empty() || self.agent.is_some() {
            return;
        }
        self.conversation.lock().unwrap().push(ChatMessage::new("user", text.clone()));
        self.transcript.push(TranscriptItem::User { text });

        let cfg = AgentConfig {
            model: self.selected_model.clone(),
            options: self.params.to_options(), // {} unless you set overrides

            system_prompt: self.system_prompt.clone(),
            tools: self.tools.clone(), // shared — create_tool mutates it mid-run
            max_steps: self.max_steps,
        };
        self.agent = Some(spawn_agent(
            self.backend.clone(),
            cfg,
            self.conversation.clone(),
            self.notify.clone(),
        ));
    }

    fn start_ingest_path(&mut self, path: String) {
        if self.ingest.is_some() {
            self.ingest_log.push("an import is already running".into());
            return;
        }
        let path = std::path::PathBuf::from(path.trim());
        let source = classify(&path);
        if let ModelSource::Unknown(p) = &source {
            self.ingest_log
                .push(format!("✗ not a recognizable model: {} (want .gguf or an HF dir)", p.display()));
            return;
        }
        self.ingest_log.push(format!("→ {:?}", source));
        self.ingest_progress = None;
        let job = IngestJob {
            source,
            llama_cpp_dir: self.llama_cpp_dir.clone(),
            quantize: self.quantize.clone(),
        };
        self.ingest = Some(spawn_ingest(self.backend.clone(), job, self.notify.clone()));
    }

    // ------------------------------------------------------------------
    fn drain_agent(&mut self) {
        let Some(handle) = self.agent.take() else { return };
        let mut finished = false;
        while let Ok(ev) = handle.events.try_recv() {
            match ev {
                AgentEvent::Token(t) => {
                    if let Some(TranscriptItem::AssistantStreaming { text }) =
                        self.transcript.last_mut()
                    {
                        text.push_str(&t);
                    } else {
                        self.transcript.push(TranscriptItem::AssistantStreaming { text: t });
                    }
                }
                AgentEvent::ToolCall { tool, args, thought } => {
                    self.pop_streaming();
                    self.transcript.push(TranscriptItem::ToolCall {
                        tool,
                        args: serde_json::to_string_pretty(&args).unwrap_or_default(),
                        thought,
                    });
                }
                AgentEvent::ToolResult { tool, output } => {
                    self.transcript.push(TranscriptItem::ToolResult { tool, output });
                }
                AgentEvent::ToolCreated(t) => {
                    save_tools(&self.tools.lock().unwrap());
                    self.transcript.push(TranscriptItem::Info {
                        text: format!("🤖 new tool created: {} — $ {}", t.name, t.command),
                    });
                }
                AgentEvent::FinalAnswer { text, thought } => {
                    self.pop_streaming();
                    self.transcript.push(TranscriptItem::Assistant { thought, text });
                }
                AgentEvent::Status(s) => self.transcript.push(TranscriptItem::Info { text: s }),
                AgentEvent::Error(e) => self.transcript.push(TranscriptItem::Error { text: e }),
                AgentEvent::Done { cancelled } => {
                    self.pop_streaming();
                    if cancelled {
                        self.transcript.push(TranscriptItem::Info {
                            text: "⏸ stopped — state kept; type a new instruction to resume".into(),
                        });
                    }
                    finished = true;
                }
            }
        }
        if !finished {
            self.agent = Some(handle);
        }
    }

    fn pop_streaming(&mut self) {
        if matches!(self.transcript.last(), Some(TranscriptItem::AssistantStreaming { .. })) {
            self.transcript.pop();
        }
    }

    fn drain_ingest(&mut self) {
        let mut done = false;
        let mut refresh = false;
        if let Some(h) = &self.ingest {
            while let Ok(ev) = h.events.try_recv() {
                match ev {
                    IngestEvent::Log(l) => self.ingest_log.push(l),
                    IngestEvent::Progress(p) => self.ingest_progress = Some(p),
                    IngestEvent::Done(name) => {
                        self.ingest_log.push(format!("✓ done: {name}"));
                        self.selected_model = name;
                        done = true;
                        refresh = true;
                    }
                    IngestEvent::Error(e) => {
                        self.ingest_log.push(format!("✗ {e}"));
                        done = true;
                    }
                }
            }
        }
        if done {
            self.ingest = None;
            self.ingest_progress = None;
        }
        if refresh {
            self.apply(Cmd::RefreshModels);
        }
        if self.ingest_log.len() > 400 {
            let drop = self.ingest_log.len() - 400;
            self.ingest_log.drain(..drop);
        }
    }

    // ------------------------------------------------------------------
    fn publish(&mut self) {
        let remote = {
            let mut store = self.remote_store.lock().unwrap();
            pairing::expire(&mut store);
            RemoteView {
                enabled: self.remote_server.is_some(),
                port: self.remote_port,
                fingerprint: self
                    .remote_server
                    .as_ref()
                    .map(|s| s.fingerprint_hex.clone())
                    .unwrap_or_default(),
                addresses: if self.remote_server.is_some() { local_addresses() } else { vec![] },
                pairing: store.pairing.as_ref().map(|p| PairingView {
                    code: p.code.clone(),
                    secs_left: p.expires.saturating_duration_since(std::time::Instant::now()).as_secs(),
                }),
                devices: store
                    .devices
                    .iter()
                    .map(|d| DeviceView {
                        id: d.id.clone(),
                        name: d.name.clone(),
                        created_unix: d.created_unix,
                    })
                    .collect(),
                connected_clients: self.remote_clients.load(Ordering::Relaxed),
                last_error: self.remote_error.clone(),
            }
        };

        let state = AppState {
            version: 0, // assigned by publish()
            status: self.status.clone(),
            models: self.models.clone(),
            selected_model: self.selected_model.clone(),
            sys: self.sys.clone(),
            params: self.params.clone(),
            system_prompt: self.system_prompt.clone(),
            max_steps: self.max_steps,
            tools: self.tools.lock().unwrap().clone(),
            transcript: self.transcript.clone(),
            agent_running: self.agent.is_some(),
            ingest: IngestView {
                running: self.ingest.is_some(),
                progress: self.ingest_progress,
                log: self.ingest_log.clone(),
                llama_cpp_dir: self.llama_cpp_dir.clone(),
                quantize: self.quantize.clone(),
            },
            remote,
        };

        // Only publish when something actually changed — cheap structural
        // compare via JSON of everything except the version counter.
        let old = self.shared.get();
        let mut old_no_v = old.clone();
        old_no_v.version = 0;
        if serde_json::to_string(&old_no_v).ok() != serde_json::to_string(&state).ok() {
            self.shared.publish(state);
            (self.notify)();
        }
    }
}

/// Best-effort list of local IPv4 addresses to show next to the pairing code.
fn local_addresses() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(o) = std::process::Command::new("sh")
        .args(["-c", "ip -o -4 addr show scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1"])
        .output()
    {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            let l = l.trim();
            if !l.is_empty() {
                out.push(l.to_string());
            }
        }
    }
    out
}
