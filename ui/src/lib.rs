//! llm-desk-ui — the one and only user interface, shared verbatim by the
//! desktop binary and the Android app.
//!
//! The UI is a pure function of an `AppState` snapshot plus a handful of
//! locally-edited text fields; every mutation is sent as a `Cmd`. Whether
//! those commands reach a controller thread in this process (desktop) or a
//! desktop across the network (phone) is invisible to this code.

use eframe::egui::{self, Color32, RichText};
use llm_desk_core::autotune::{OverrideField, ParamOverrides};
use llm_desk_core::controller::ControllerHandle;
use llm_desk_core::protocol::{AppState, Cmd, TranscriptItem};
use llm_desk_core::remote::client::{LinkStatus, RemoteSession};
use llm_desk_core::tools::{ParamType, ToolDef, ToolKind, ToolParam};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Frontend: where do state and commands go?
// ---------------------------------------------------------------------------

pub enum Frontend {
    /// In-process controller (the desktop).
    Local(ControllerHandle),
    /// TLS WebSocket to a desktop (the phone).
    Remote(Arc<RemoteSession>),
}

impl Frontend {
    fn state(&self) -> AppState {
        match self {
            Frontend::Local(h) => h.shared.get(),
            Frontend::Remote(s) => s.shared.get(),
        }
    }
    fn send(&self, cmd: Cmd) {
        match self {
            Frontend::Local(h) => h.send(cmd),
            Frontend::Remote(s) => s.send(cmd),
        }
    }
    fn add_notifier(&self, n: llm_desk_core::protocol::Notify) {
        match self {
            Frontend::Local(h) => h.add_notifier(n),
            Frontend::Remote(s) => s.add_notifier(n),
        }
    }
    fn is_remote(&self) -> bool {
        matches!(self, Frontend::Remote(_))
    }
    /// Send a model file to the desktop for import. On the desktop this is just
    /// a local import; on the phone the bytes stream up the TLS socket.
    fn import_or_upload_path(&self, path: std::path::PathBuf) {
        match self {
            Frontend::Local(h) => h.send(Cmd::ImportPath { path: path.display().to_string() }),
            Frontend::Remote(s) => s.upload_model_path(path),
        }
    }
    /// Upload model bytes the phone handed us (e.g. from a file picker/drop).
    fn upload_bytes(&self, name: String, bytes: Vec<u8>) {
        if let Frontend::Remote(s) = self {
            s.upload_model_bytes(name, bytes);
        }
    }
    /// Client-side upload progress (sent, total), if a transfer is running.
    fn upload_progress(&self) -> Option<(u64, u64)> {
        match self {
            Frontend::Remote(s) => s
                .link
                .lock()
                .unwrap()
                .upload
                .as_ref()
                .map(|u| (u.sent, u.total)),
            Frontend::Local(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------

struct ToolEditor {
    name: String,
    description: String,
    command: String,
    params: Vec<ToolParam>,
}

impl Default for ToolEditor {
    fn default() -> Self {
        Self { name: String::new(), description: String::new(), command: String::new(), params: vec![] }
    }
}

/// The phone's connection form.
#[derive(Default)]
struct ConnectForm {
    host: String,
    port: String,
    code: String,
    device_name: String,
}

pub struct UiApp {
    frontend: Frontend,
    mobile: bool,
    show_controls: bool, // mobile: toggle between controls and chat

    // one-time init flags
    registered_repaint: bool,
    synced_fields: bool,

    // locally-edited fields (pushed as Cmds on change; seeded from the first
    // snapshot so remote sessions don't fight the keyboard)
    input: String,
    params: ParamOverrides,
    system_prompt: String,
    max_steps: usize,
    tool_editor: ToolEditor,
    pull_name: String,
    drop_path: String,
    llama_cpp_dir: String,
    quantize: String,
    remote_port: String,

    connect: ConnectForm,

    // Android soft keyboard hook: called with `true` when egui wants text.
    kb_hook: Option<Box<dyn Fn(bool)>>,
    kb_last: bool,

    // Android file-picker hook: called to launch the system document picker so
    // the user can choose a model file to upload. The picked file is streamed
    // up by the Android layer itself.
    file_pick_hook: Option<Box<dyn Fn()>>,
}

impl UiApp {
    pub fn new(frontend: Frontend, mobile: bool) -> Self {
        Self {
            frontend,
            mobile,
            show_controls: false,
            registered_repaint: false,
            synced_fields: false,
            input: String::new(),
            params: ParamOverrides::default(),
            system_prompt: String::new(),
            max_steps: 16,
            tool_editor: ToolEditor::default(),
            pull_name: String::new(),
            drop_path: String::new(),
            llama_cpp_dir: String::new(),
            quantize: String::new(),
            remote_port: String::new(),
            connect: ConnectForm::default(),
            kb_hook: None,
            kb_last: false,
            file_pick_hook: None,
        }
    }

    pub fn with_keyboard_hook(mut self, hook: Box<dyn Fn(bool)>) -> Self {
        self.kb_hook = Some(hook);
        self
    }

    /// Provide a way to launch the platform's file picker (Android). When set,
    /// the phone's Model-import panel shows a "Choose a file to upload" button.
    pub fn with_file_pick_hook(mut self, hook: Box<dyn Fn()>) -> Self {
        self.file_pick_hook = Some(hook);
        self
    }

    fn send(&self, cmd: Cmd) {
        self.frontend.send(cmd);
    }
}

impl eframe::App for UiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.registered_repaint {
            self.registered_repaint = true;
            let ctx2 = ctx.clone();
            self.frontend.add_notifier(Arc::new(move || ctx2.request_repaint()));
        }

        // Android: pop the soft keyboard when a text field gains focus.
        let wants_kb = ctx.wants_keyboard_input();
        if wants_kb != self.kb_last {
            self.kb_last = wants_kb;
            if let Some(hook) = &self.kb_hook {
                hook(wants_kb);
            }
        }

        // Phone not connected → connection screen instead of the app.
        if let Frontend::Remote(session) = &self.frontend {
            let status = session.link.lock().unwrap().status.clone();
            if status != LinkStatus::Connected {
                let session = session.clone();
                self.connect_screen(ctx, &session, status);
                return;
            }
        }

        let st = self.frontend.state();

        // Seed locally-edited fields from the first real snapshot.
        if !self.synced_fields && st.version > 0 {
            self.synced_fields = true;
            self.params = st.params.clone();
            self.system_prompt = st.system_prompt.clone();
            self.max_steps = st.max_steps;
            self.llama_cpp_dir = st.ingest.llama_cpp_dir.clone();
            self.quantize = st.ingest.quantize.clone();
            self.remote_port = st.remote.port.to_string();
        }

        // OS file drops anywhere in the window. On the desktop the dropped path
        // is imported locally; on the phone the file is streamed up to the
        // desktop (by path if we got one, otherwise by the bytes egui captured).
        let dropped: Vec<_> = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            if let Some(path) = f.path {
                self.frontend.import_or_upload_path(path);
            } else if self.frontend.is_remote() {
                if let Some(bytes) = f.bytes {
                    let name = if f.name.is_empty() { "model.gguf".into() } else { f.name.clone() };
                    self.frontend.upload_bytes(name, bytes.to_vec());
                }
            }
        }

        if self.mobile {
            egui::TopBottomPanel::top("mobile_bar").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let label = if self.show_controls { "💬 chat" } else { "☰ controls" };
                    if ui.button(label).clicked() {
                        self.show_controls = !self.show_controls;
                    }
                    ui.label(RichText::new(&st.status).small().italics().color(Color32::GRAY));
                });
            });
            if self.show_controls {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| self.controls(ui, &st));
                });
            } else {
                self.chat_panel(ctx, &st);
            }
        } else {
            egui::SidePanel::left("controls").min_width(360.0).show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new(&st.status).italics().color(Color32::GRAY));
                    ui.separator();
                    self.controls(ui, &st);
                });
            });
            self.chat_panel(ctx, &st);
        }

        if st.agent_running || st.ingest.running {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

impl UiApp {
    fn controls(&mut self, ui: &mut egui::Ui, st: &AppState) {
        self.model_section(ui, st);
        self.sysinfo_section(ui, st);
        self.params_section(ui, st);
        self.prompt_section(ui);
        self.tools_section(ui, st);
        self.import_section(ui, st);
        self.remote_section(ui, st);
    }

    // ------------------------------------------------------------------
    fn model_section(&mut self, ui: &mut egui::Ui, st: &AppState) {
        egui::CollapsingHeader::new("Model").default_open(true).show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut selected = st.selected_model.clone();
                egui::ComboBox::from_id_salt("model_select")
                    .width(240.0)
                    .selected_text(if selected.is_empty() { "— none —" } else { &selected })
                    .show_ui(ui, |ui| {
                        for m in &st.models {
                            ui.selectable_value(&mut selected, m.name.clone(), &m.name);
                        }
                    });
                if selected != st.selected_model {
                    self.send(Cmd::SelectModel { name: selected });
                }
                if ui.button("⟳").on_hover_text("Refresh model list").clicked() {
                    self.send(Cmd::RefreshModels);
                }
            });
            if let Some(m) = st.models.iter().find(|m| m.name == st.selected_model) {
                ui.label(
                    RichText::new(format!(
                        "{} · {} · {:.1} GB",
                        m.parameter_size,
                        m.quantization,
                        m.size_bytes as f64 / 1e9
                    ))
                    .small()
                    .color(Color32::GRAY),
                );
            }
        });
    }

    fn sysinfo_section(&mut self, ui: &mut egui::Ui, st: &AppState) {
        egui::CollapsingHeader::new("System (read-only, informational)")
            .default_open(true)
            .show(ui, |ui| {
                let s = &st.sys;
                ui.label(format!("CPU: {}", s.cpu_model));
                ui.label(format!(
                    "Cores: {} physical / {} logical",
                    s.physical_cores, s.logical_cpus
                ));
                ui.label(format!(
                    "RAM: {:.1} GB total, {:.1} GB available",
                    s.ram_total_mb as f64 / 1024.0,
                    s.ram_available_mb as f64 / 1024.0
                ));
                match &s.gpu {
                    Some(g) => {
                        ui.label(format!("GPU: {}", g.name));
                        if g.vram_total_mb > 0 {
                            ui.label(format!(
                                "VRAM: {:.1} GB total, {:.1} GB free",
                                g.vram_total_mb as f64 / 1024.0,
                                g.vram_free_mb as f64 / 1024.0
                            ));
                        }
                    }
                    None => {
                        ui.label("GPU: none detected (CPU inference)");
                    }
                }
                if ui.button("Re-detect").clicked() {
                    self.send(Cmd::RedetectSystem);
                }
            });
    }

    fn params_section(&mut self, ui: &mut egui::Ui, _st: &AppState) {
        egui::CollapsingHeader::new("Parameters (blank = Ollama decides)")
            .default_open(true)
            .show(ui, |ui| {
                let mut changed = false;
                egui::Grid::new("params_grid").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                    changed |= override_row(ui, "num_gpu (GPU layers)", &mut self.params.num_gpu);
                    changed |= override_row(ui, "num_thread", &mut self.params.num_thread);
                    changed |= override_row(ui, "num_ctx (context)", &mut self.params.num_ctx);
                    changed |= override_row(ui, "temperature", &mut self.params.temperature);
                    changed |= override_row(ui, "top_p", &mut self.params.top_p);
                    changed |= override_row(ui, "top_k", &mut self.params.top_k);
                    changed |= override_row(ui, "repeat_penalty", &mut self.params.repeat_penalty);
                    changed |= override_row(ui, "num_predict (max tokens)", &mut self.params.num_predict);
                });
                if changed {
                    self.send(Cmd::SetParams { params: self.params.clone() });
                }
                ui.horizontal(|ui| {
                    ui.label("max agent steps:");
                    if ui.add(egui::DragValue::new(&mut self.max_steps).range(1..=64)).changed() {
                        self.send(Cmd::SetMaxSteps { steps: self.max_steps });
                    }
                });
                ui.label(
                    RichText::new("the app does not tune anything — blank fields are omitted so Ollama uses its own defaults; type a value to force it")
                        .small()
                        .color(Color32::GRAY),
                );
            });
    }

    fn prompt_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("System prompt").default_open(true).show(ui, |ui| {
            if ui
                .add(
                    egui::TextEdit::multiline(&mut self.system_prompt)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY)
                        .hint_text("System prompt…"),
                )
                .changed()
            {
                self.send(Cmd::SetSystemPrompt { text: self.system_prompt.clone() });
            }
            ui.label(
                RichText::new("a tool-use block is auto-appended when tools are enabled")
                    .small()
                    .color(Color32::GRAY),
            );
        });
    }

    fn tools_section(&mut self, ui: &mut egui::Ui, st: &AppState) {
        egui::CollapsingHeader::new("Tools").default_open(false).show(ui, |ui| {
            for t in &st.tools {
                ui.horizontal(|ui| {
                    let mut enabled = t.enabled;
                    if ui.checkbox(&mut enabled, "").changed() {
                        self.send(Cmd::SetToolEnabled { name: t.name.clone(), enabled });
                    }
                    ui.label(RichText::new(&t.name).strong());
                    if t.ai_created {
                        ui.label(RichText::new("🤖").small()).on_hover_text("created by the model");
                    }
                    if t.kind == ToolKind::CreateTool {
                        ui.label(RichText::new("meta").small().color(Color32::KHAKI))
                            .on_hover_text("lets the model add new tools to this registry");
                    }
                    if ui.small_button("✕").clicked() {
                        self.send(Cmd::RemoveTool { name: t.name.clone() });
                    }
                });
                ui.label(RichText::new(&t.description).small().color(Color32::GRAY));
                if t.kind == ToolKind::Shell {
                    ui.label(RichText::new(format!("$ {}", t.command)).small().monospace());
                }
                ui.add_space(4.0);
            }
            ui.separator();
            ui.label(RichText::new("Add a tool").strong());
            let ed = &mut self.tool_editor;
            ui.add(egui::TextEdit::singleline(&mut ed.name).hint_text("name (snake_case)"));
            ui.add(egui::TextEdit::singleline(&mut ed.description).hint_text("description for the model"));
            ui.add(
                egui::TextEdit::singleline(&mut ed.command)
                    .hint_text("shell command, e.g. curl -s 'https://wttr.in/{city}?format=3'"),
            );
            let mut remove_param: Option<usize> = None;
            for (i, p) in ed.params.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut p.name).desired_width(80.0).hint_text("param"));
                    egui::ComboBox::from_id_salt(("ptype", i))
                        .width(70.0)
                        .selected_text(p.ptype.json_name())
                        .show_ui(ui, |ui| {
                            for t in ParamType::ALL {
                                ui.selectable_value(&mut p.ptype, t, t.json_name());
                            }
                        });
                    ui.checkbox(&mut p.required, "req");
                    if ui.small_button("✕").clicked() {
                        remove_param = Some(i);
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut p.description)
                        .desired_width(f32::INFINITY)
                        .hint_text("param description"),
                );
            }
            if let Some(i) = remove_param {
                ed.params.remove(i);
            }
            ui.horizontal(|ui| {
                if ui.button("+ parameter").clicked() {
                    ed.params.push(ToolParam {
                        name: String::new(),
                        description: String::new(),
                        ptype: ParamType::String,
                        required: true,
                    });
                }
                let ready = !ed.name.trim().is_empty() && !ed.command.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new("Add tool")).clicked() {
                    self.frontend.send(Cmd::AddTool {
                        tool: ToolDef {
                            name: ed.name.trim().to_string(),
                            description: ed.description.trim().to_string(),
                            params: ed.params.clone(),
                            command: ed.command.trim().to_string(),
                            enabled: true,
                            kind: ToolKind::Shell,
                            ai_created: false,
                        },
                    });
                    *ed = ToolEditor::default();
                }
            });
            ui.label(
                RichText::new(
                    "system-prompt block + output JSON schema regenerate automatically. \
                     tools persist to ~/.config/llm-desk/tools.json. \
                     ask the model to build tools for you — it uses `create_tool`. \
                     ⚠ tools run shell commands with your privileges.",
                )
                .small()
                .color(Color32::GRAY),
            );
        });
    }

    fn import_section(&mut self, ui: &mut egui::Ui, st: &AppState) {
        let remote = self.frontend.is_remote();
        egui::CollapsingHeader::new("Model import").default_open(false).show(ui, |ui| {
            // drop zone (works on the desktop; on the phone it uploads)
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 64.0),
                egui::Sense::hover(),
            );
            let hovering_files = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
            let stroke = if hovering_files {
                egui::Stroke::new(2.0, Color32::LIGHT_GREEN)
            } else {
                egui::Stroke::new(1.0, Color32::DARK_GRAY)
            };
            ui.painter().rect_stroke(rect, 8.0, stroke);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                if remote {
                    "drop or choose a .gguf to upload to the desktop"
                } else {
                    "drop a .gguf file or a HuggingFace model folder here"
                },
                egui::FontId::proportional(13.0),
                Color32::GRAY,
            );
            ui.add_space(6.0);

            // Phone: pick a file on the device and stream it to the desktop.
            if remote {
                ui.horizontal(|ui| {
                    let can_pick = self.file_pick_hook.is_some();
                    if ui
                        .add_enabled(can_pick, egui::Button::new("📁 Choose a model file to upload"))
                        .clicked()
                    {
                        if let Some(hook) = &self.file_pick_hook {
                            hook();
                        }
                    }
                });
                if let Some((sent, total)) = self.frontend.upload_progress() {
                    let frac = if total > 0 { sent as f32 / total as f32 } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac).show_percentage().text(format!(
                        "uploading… {:.0}/{:.0} MB",
                        sent as f64 / 1_048_576.0,
                        total as f64 / 1_048_576.0
                    )));
                }
                ui.add_space(4.0);
            }

            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.drop_path)
                        .desired_width(230.0)
                        .hint_text(if remote {
                            "…or a path on this phone"
                        } else {
                            "…or type a path (on the desktop)"
                        }),
                );
                if ui.button("Import").clicked() && !self.drop_path.trim().is_empty() {
                    self.frontend
                        .import_or_upload_path(std::path::PathBuf::from(self.drop_path.trim()));
                }
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.pull_name)
                        .desired_width(230.0)
                        .hint_text("registry name, e.g. llama3.2:3b"),
                );
                if ui.button("Pull").clicked() && !self.pull_name.trim().is_empty() && !st.ingest.running {
                    self.frontend.send(Cmd::Pull { name: self.pull_name.trim().to_string() });
                }
            });
            ui.horizontal(|ui| {
                ui.label("llama.cpp dir:");
                if ui
                    .add(egui::TextEdit::singleline(&mut self.llama_cpp_dir).desired_width(180.0))
                    .changed()
                {
                    self.send(Cmd::SetLlamaCppDir { dir: self.llama_cpp_dir.clone() });
                }
            });
            ui.horizontal(|ui| {
                ui.label("quantize HF to:");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.quantize)
                            .desired_width(90.0)
                            .hint_text("blank = f16"),
                    )
                    .changed()
                {
                    self.send(Cmd::SetQuantize { quantize: self.quantize.clone() });
                }
            });

            if let Some(p) = st.ingest.progress {
                ui.add(egui::ProgressBar::new(p).show_percentage());
            }
            if st.ingest.running && ui.button("Cancel import").clicked() {
                self.send(Cmd::CancelImport);
            }
            if !st.ingest.log.is_empty() {
                egui::ScrollArea::vertical()
                    .id_salt("ingest_log")
                    .max_height(120.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for l in &st.ingest.log {
                            ui.label(RichText::new(l).small().monospace());
                        }
                    });
            }
        });
    }

    fn remote_section(&mut self, ui: &mut egui::Ui, st: &AppState) {
        egui::CollapsingHeader::new("Remote access (phone)").default_open(false).show(ui, |ui| {
            let r = &st.remote;
            ui.horizontal(|ui| {
                let mut enabled = r.enabled;
                if ui.checkbox(&mut enabled, "enable server").changed() {
                    self.send(Cmd::SetRemoteEnabled { enabled });
                }
                if !r.enabled {
                    ui.label("port:");
                    if ui
                        .add(egui::TextEdit::singleline(&mut self.remote_port).desired_width(60.0))
                        .changed()
                    {
                        if let Ok(p) = self.remote_port.trim().parse::<u16>() {
                            self.send(Cmd::SetRemotePort { port: p });
                        }
                    }
                } else {
                    ui.label(format!("port {} · {} client(s)", r.port, r.connected_clients));
                }
            });
            if !r.last_error.is_empty() {
                ui.label(RichText::new(&r.last_error).color(Color32::LIGHT_RED));
            }
            if r.enabled {
                if !r.addresses.is_empty() {
                    ui.label(
                        RichText::new(format!("reachable at: {}", r.addresses.join(", ")))
                            .small()
                            .color(Color32::GRAY),
                    );
                }
                if r.fingerprint.len() == 64 {
                    ui.label(
                        RichText::new(format!(
                            "cert fingerprint: {}…{}",
                            &r.fingerprint[..8],
                            &r.fingerprint[56..]
                        ))
                        .small()
                        .monospace()
                        .color(Color32::GRAY),
                    );
                }
                match &r.pairing {
                    Some(p) => {
                        ui.add_space(4.0);
                        ui.label("enter this code on the phone (one-time, expires):");
                        ui.label(
                            RichText::new(&p.code)
                                .size(20.0)
                                .monospace()
                                .color(Color32::LIGHT_GREEN),
                        );
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} s left", p.secs_left)).small().color(Color32::GRAY));
                            if ui.small_button("cancel").clicked() {
                                self.send(Cmd::CancelPairing);
                            }
                        });
                    }
                    None => {
                        if ui.button("Pair new device").clicked() {
                            self.send(Cmd::StartPairing);
                        }
                    }
                }
                if !r.devices.is_empty() {
                    ui.add_space(4.0);
                    ui.label(RichText::new("paired devices").strong());
                    for d in &r.devices {
                        ui.horizontal(|ui| {
                            ui.label(&d.name);
                            ui.label(RichText::new(format!("#{}", d.id)).small().monospace().color(Color32::GRAY));
                            if ui.small_button("revoke").clicked() {
                                self.send(Cmd::RevokeDevice { id: d.id.clone() });
                            }
                        });
                    }
                }
                ui.label(
                    RichText::new(
                        "phones pin this server's certificate at pairing and authenticate with \
                         a revocable token. For access from other networks, put both devices on \
                         a Tailscale/WireGuard network — see README.",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
            }
        });
    }

    // ------------------------------------------------------------------
    fn chat_panel(&mut self, ctx: &egui::Context, st: &AppState) {
        egui::TopBottomPanel::bottom("input_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let running = st.agent_running;
                let hint = if running {
                    "agent is running — press Stop to interrupt, then type to resume"
                } else {
                    "instruction for the agent…"
                };
                let te = ui.add_sized(
                    [ui.available_width() - 150.0, 28.0],
                    egui::TextEdit::singleline(&mut self.input).hint_text(hint),
                );
                let enter = te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                if running {
                    if ui
                        .add_sized([70.0, 28.0], egui::Button::new(RichText::new("■ Stop").color(Color32::LIGHT_RED)))
                        .clicked()
                    {
                        self.send(Cmd::Stop);
                    }
                } else if ui.add_sized([70.0, 28.0], egui::Button::new("▶ Send")).clicked() || enter {
                    let text = self.input.trim().to_string();
                    if !text.is_empty() {
                        self.input.clear();
                        self.send(Cmd::SendPrompt { text });
                    }
                }
                if ui
                    .add_sized([64.0, 28.0], egui::Button::new("Clear"))
                    .on_hover_text("Clear conversation")
                    .clicked()
                    && !st.agent_running
                {
                    self.send(Cmd::ClearConversation);
                }
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for item in &st.transcript {
                    match item {
                        TranscriptItem::User { text } => {
                            ui.add_space(6.0);
                            ui.label(RichText::new("you").small().strong().color(Color32::LIGHT_BLUE));
                            ui.label(text);
                        }
                        TranscriptItem::AssistantStreaming { text } => {
                            ui.add_space(6.0);
                            ui.label(RichText::new("assistant (streaming…)").small().strong());
                            ui.label(RichText::new(text).monospace().small());
                        }
                        TranscriptItem::Assistant { thought, text } => {
                            ui.add_space(6.0);
                            ui.label(RichText::new("assistant").small().strong().color(Color32::LIGHT_GREEN));
                            if !thought.is_empty() {
                                ui.label(RichText::new(format!("💭 {thought}")).small().italics().color(Color32::GRAY));
                            }
                            ui.label(text);
                        }
                        TranscriptItem::ToolCall { tool, args, thought } => {
                            ui.add_space(6.0);
                            if !thought.is_empty() {
                                ui.label(RichText::new(format!("💭 {thought}")).small().italics().color(Color32::GRAY));
                            }
                            ui.label(
                                RichText::new(format!("🔧 {tool}({args})"))
                                    .small()
                                    .monospace()
                                    .color(Color32::YELLOW),
                            );
                        }
                        TranscriptItem::ToolResult { tool, output } => {
                            egui::CollapsingHeader::new(format!("↩ {tool} result"))
                                .default_open(false)
                                .show(ui, |ui| {
                                    ui.label(RichText::new(output).small().monospace());
                                });
                        }
                        TranscriptItem::Info { text } => {
                            ui.add_space(4.0);
                            ui.label(RichText::new(text).small().italics().color(Color32::GRAY));
                        }
                        TranscriptItem::Error { text } => {
                            ui.add_space(4.0);
                            ui.label(RichText::new(format!("✗ {text}")).color(Color32::LIGHT_RED));
                        }
                    }
                }
            });
        });
    }

    // ------------------------------------------------------------------
    fn connect_screen(&mut self, ctx: &egui::Context, session: &Arc<RemoteSession>, status: LinkStatus) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.heading("llm-desk");
                ui.label(RichText::new("remote control for your desktop").color(Color32::GRAY));
            });
            ui.add_space(16.0);

            let saved = session.link.lock().unwrap().saved.clone();

            if let Some(cfg) = &saved {
                ui.label(format!("paired with {}:{} as \"{}\"", cfg.host, cfg.port, cfg.device_name));
                ui.horizontal(|ui| {
                    if ui.button("▶ Connect").clicked() {
                        session.connect_saved();
                    }
                    if ui.button("Forget this desktop").clicked() {
                        session.forget_server();
                    }
                });
                ui.add_space(12.0);
                ui.separator();
                ui.label(RichText::new("…or pair again:").small().color(Color32::GRAY));
            } else {
                ui.label(
                    "On the desktop: open “Remote access”, enable the server, press \
                     “Pair new device”, then enter what it shows:",
                );
            }
            ui.add_space(8.0);

            egui::Grid::new("connect_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("desktop address:");
                ui.add(egui::TextEdit::singleline(&mut self.connect.host).hint_text("192.168.1.20 or Tailscale IP"));
                ui.end_row();
                ui.label("port:");
                ui.add(egui::TextEdit::singleline(&mut self.connect.port).hint_text("4832"));
                ui.end_row();
                ui.label("pairing code:");
                ui.add(egui::TextEdit::singleline(&mut self.connect.code).hint_text("XXXX-XXXX-XXXX-XXXX"));
                ui.end_row();
                ui.label("this device's name:");
                ui.add(egui::TextEdit::singleline(&mut self.connect.device_name).hint_text("my phone"));
                ui.end_row();
            });
            ui.add_space(8.0);

            let ready = !self.connect.host.trim().is_empty() && !self.connect.code.trim().is_empty();
            if ui.add_enabled(ready, egui::Button::new("🔗 Pair & connect")).clicked() {
                let port = self.connect.port.trim().parse::<u16>().unwrap_or(4832);
                let name = if self.connect.device_name.trim().is_empty() {
                    "phone".to_string()
                } else {
                    self.connect.device_name.trim().to_string()
                };
                session.connect_pair(
                    self.connect.host.trim().to_string(),
                    port,
                    self.connect.code.trim().to_string(),
                    name,
                );
            }

            ui.add_space(12.0);
            match status {
                LinkStatus::Connecting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("connecting…");
                    });
                }
                LinkStatus::Error(e) => {
                    ui.label(RichText::new(format!("✗ {e}")).color(Color32::LIGHT_RED));
                }
                _ => {}
            }
        });
    }
}

/// One row of the parameter grid: label | text box | forced/default tag.
/// Returns true if the field was edited this frame.
fn override_row(ui: &mut egui::Ui, label: &str, field: &mut OverrideField) -> bool {
    ui.label(label);
    let changed = ui
        .add(
            egui::TextEdit::singleline(&mut field.manual)
                .desired_width(70.0)
                .hint_text("auto"),
        )
        .changed();
    if field.is_manual() {
        ui.label(RichText::new("forced").small().color(Color32::LIGHT_YELLOW));
    } else {
        ui.label(RichText::new("Ollama default").small().color(Color32::GRAY));
    }
    ui.end_row();
    changed
}
