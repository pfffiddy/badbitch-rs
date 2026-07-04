//! badbitch-gui — native desktop control panel for badbitch-rs (egui/eframe).
//!
//! Three tabs — Settings, Prompt, Run — plus a toggleable "Thought process" window that
//! shows the model's reasoning, every command/tool it invokes, and per-turn perf/hardware,
//! with its own verbosity filter. Settings exposes every agent param, the model picker
//! (from your installed Ollama models), a thinking On/Off/Default toggle, and the full set
//! of Ollama generation options. Saving writes `~/.config/badbitch-rs/config.ini`.

use std::sync::mpsc::{Receiver, Sender, channel};

use badbitch::agent::AgentEvent;
use badbitch::config::{API_KEY_NAMES, Config, write_ini};
use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 820.0])
            .with_title("badbitch-rs"),
        ..Default::default()
    };
    eframe::run_native(
        "badbitch-rs",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Settings,
    Prompt,
    Run,
}

#[derive(PartialEq, Clone, Copy)]
enum ThinkMode {
    Default,
    On,
    Off,
}

/// Verbosity filter for the event views.
#[derive(PartialEq, Clone, Copy)]
enum Verbosity {
    Quiet,   // final answers + notices
    Normal,  // + tool calls + perf/hardware
    Verbose, // + thinking + tool results
}

const BOOL_KEYS: &[&str] = &["prefetch_recon", "verbose", "summary", "tor"];

// [osint] keys we surface, in a friendly order.
const OSINT_KEYS: &[&str] = &[
    "searxng_url",
    "max_tool_iters",
    "max_continuations",
    "http_timeout",
    "shell_timeout",
    "long_tool_timeout",
    "max_tool_result_chars",
    "max_fetch_chars",
    "geocode_countrycodes",
    "prefetch_recon",
    "summary",
    "verbose",
    "tor",
    "tor_proxy",
    "case_db",
    "audit_log",
];

// Ollama generation options (everything `ollama run` supports). num_ctx/temperature/top_p/
// repeat_penalty are the canonical four (written to [osint]); the rest go to [model_options].
const GEN_KEYS: &[&str] = &[
    "num_ctx",
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "typical_p",
    "repeat_penalty",
    "repeat_last_n",
    "presence_penalty",
    "frequency_penalty",
    "num_predict",
    "seed",
    "mirostat",
    "mirostat_tau",
    "mirostat_eta",
    "num_gpu",
    "num_thread",
    "num_keep",
    "num_batch",
    "stop",
];
const CANONICAL_GEN: &[&str] = &["num_ctx", "temperature", "top_p", "repeat_penalty"];

fn describe(key: &str) -> &'static str {
    match key {
        "num_ctx" => "Context window (tokens). Larger = more memory + VRAM. 20480 fits a 12GB GPU for a 14B.",
        "temperature" => "Randomness. Lower = focused/deterministic, higher = creative. 0.2–0.7 typical.",
        "top_p" => "Nucleus sampling cutoff (0–1). 0.9 typical.",
        "top_k" => "Sample only from the top-K tokens. 40 typical; 0 = disabled.",
        "min_p" => "Min probability relative to the top token (0–1). Alternative to top_p.",
        "typical_p" => "Locally-typical sampling (0–1).",
        "repeat_penalty" => "Penalize repetition. 1.1 typical; >1.2 can hurt quality.",
        "repeat_last_n" => "How many recent tokens the repeat penalty considers. 64 typical.",
        "presence_penalty" => "Penalize tokens already present (OpenAI-style).",
        "frequency_penalty" => "Penalize tokens by frequency (OpenAI-style).",
        "num_predict" => "Max tokens to generate (-1 = until stop / context).",
        "seed" => "RNG seed for reproducible output (integer).",
        "mirostat" => "Mirostat sampling: 0 off, 1 or 2.",
        "mirostat_tau" => "Mirostat target entropy (default 5.0).",
        "mirostat_eta" => "Mirostat learning rate (default 0.1).",
        "num_gpu" => "Number of model layers to offload to GPU (-1/blank = auto).",
        "num_thread" => "CPU threads for the parts not on GPU.",
        "num_keep" => "Tokens from the prompt to always keep when the window fills.",
        "num_batch" => "Prompt batch size.",
        "stop" => "Stop sequences (comma-separated).",
        "searxng_url" => "Local SearXNG endpoint used by web_search.",
        "max_tool_iters" => "Max tool calls per turn before a forced wrap-up.",
        "max_continuations" => "How many extra tool batches after hitting the cap.",
        "prefetch_recon" => "Gather a recon corpus before the model's first turn.",
        "summary" => "Prepend a 3–5 bullet TL;DR to each answer.",
        "verbose" => "Per-tool timing + perf/hardware on screen and in the debug log.",
        "tor" => "Route scraping through the Tor SOCKS proxy.",
        "case_db" => "SQLite case store path.",
        "audit_log" => "Append-only tool audit log path.",
        _ => "",
    }
}

struct App {
    tab: Tab,

    // ── settings (editable strings) ──
    model: String,
    ollama_host: String,
    think: ThinkMode,
    osint: Vec<(String, String)>, // OSINT_KEYS order
    gen_opts: Vec<(String, String)>,   // GEN_KEYS order
    keys: Vec<(String, String)>,  // API_KEY_NAMES order
    models: Vec<String>,          // installed Ollama models
    settings_status: String,

    // ── prompt ──
    prompt_text: String,
    prompt_status: String,

    // ── run ──
    target: String,
    events: Vec<AgentEvent>,
    rx: Option<Receiver<AgentEvent>>,
    running: bool,
    run_verbosity: Verbosity,

    // ── thought window ──
    show_thoughts: bool,
    thoughts_verbosity: Verbosity,
}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = Config::load();
        let models = fetch_models(&cfg.ollama_host);

        let osint = OSINT_KEYS
            .iter()
            .map(|k| ((*k).to_string(), osint_value(&cfg, k)))
            .collect();
        let gen_opts = GEN_KEYS
            .iter()
            .map(|k| ((*k).to_string(), gen_value(&cfg, k)))
            .collect();
        let keys = API_KEY_NAMES
            .iter()
            .map(|k| ((*k).to_string(), cfg.key(k)))
            .collect();
        let think = match cfg.think {
            None => ThinkMode::Default,
            Some(true) => ThinkMode::On,
            Some(false) => ThinkMode::Off,
        };

        App {
            tab: Tab::Run,
            model: cfg.model.clone(),
            ollama_host: cfg.ollama_host.clone(),
            think,
            osint,
            gen_opts,
            keys,
            models,
            settings_status: String::new(),
            prompt_text: badbitch::prompt::base_prompt(),
            prompt_status: String::new(),
            target: String::new(),
            events: Vec::new(),
            rx: None,
            running: false,
            run_verbosity: Verbosity::Normal,
            show_thoughts: false,
            thoughts_verbosity: Verbosity::Verbose,
        }
    }

    fn save_settings(&mut self) {
        let mut osint_kv: Vec<(String, String)> = Vec::new();
        // canonical generation four live in [osint]
        for k in CANONICAL_GEN {
            if let Some(v) = self.gen_opts.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
                && !v.trim().is_empty()
            {
                osint_kv.push(((*k).to_string(), v));
            }
        }
        for (k, v) in &self.osint {
            osint_kv.push((k.clone(), v.clone()));
        }
        if !self.ollama_host.trim().is_empty() {
            osint_kv.push(("ollama_host".into(), self.ollama_host.clone()));
        }
        match self.think {
            ThinkMode::On => osint_kv.push(("think".into(), "true".into())),
            ThinkMode::Off => osint_kv.push(("think".into(), "false".into())),
            ThinkMode::Default => {}
        }

        // extra generation options -> [model_options]
        let mopts: Vec<(String, String)> = self
            .gen_opts
            .iter()
            .filter(|(k, v)| !CANONICAL_GEN.contains(&k.as_str()) && !v.trim().is_empty())
            .cloned()
            .collect();

        let keys_kv: Vec<(String, String)> = self.keys.clone();

        let sections: Vec<(&str, Vec<(String, String)>)> = vec![
            ("model", vec![("name".to_string(), self.model.clone())]),
            ("osint", osint_kv),
            ("model_options", mopts),
            ("api_keys", keys_kv),
        ];

        match write_ini(&Config::config_path(), &sections) {
            Ok(()) => {
                self.settings_status = format!("Saved to {}", Config::config_path().display());
            }
            Err(e) => self.settings_status = format!("Save failed: {e}"),
        }
    }

    fn start_run(&mut self) {
        if self.running {
            return;
        }
        let target = self.target.trim().to_string();
        if target.is_empty() {
            return;
        }
        self.events.clear();
        let (tx, rx): (Sender<AgentEvent>, Receiver<AgentEvent>) = channel();
        self.rx = Some(rx);
        self.running = true;

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(AgentEvent::Final(format!("[gui] runtime error: {e}")));
                    return;
                }
            };
            rt.block_on(async {
                let cfg = std::sync::Arc::new(Config::load());
                badbitch::debug::init(&badbitch::debug_log_path());
                let ctx = badbitch::build_context(&cfg);
                let client = badbitch::ollama::OllamaClient::new(&cfg);
                let router = badbitch::tool::toolset();
                let workdir = std::env::current_dir().unwrap_or_default();
                let mut messages = vec![badbitch::ollama::ChatMessage::system(
                    badbitch::prompt::system_prompt(&cfg, &workdir),
                )];
                badbitch::agent::preflight(&ctx, &cfg, &mut messages, &target).await;
                messages.push(badbitch::ollama::ChatMessage::user(target.clone()));
                let ans = badbitch::agent::run_turn_streaming(
                    &client,
                    &router,
                    &ctx,
                    &cfg,
                    &mut messages,
                    Some(&tx),
                )
                .await;
                let _ = tx.send(AgentEvent::Final(ans));
            });
        });
    }

    fn drain_events(&mut self) {
        let mut done = false;
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                if matches!(ev, AgentEvent::Final(_)) {
                    done = true;
                }
                self.events.push(ev);
            }
        }
        if done {
            self.running = false;
        }
    }
}

/// Fetch installed Ollama model names synchronously (used at startup and by the Refresh
/// button). Blocks briefly on a throwaway runtime — fine for a one-shot action.
fn fetch_models(host: &str) -> Vec<String> {
    let host = host.to_string();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| rt.block_on(badbitch::ollama::list_models(&host)))
            .unwrap_or_default()
    })
    .join()
    .unwrap_or_default()
}

fn osint_value(cfg: &Config, key: &str) -> String {
    match key {
        "searxng_url" => cfg.searx_url.clone(),
        "max_tool_iters" => cfg.max_tool_iters.to_string(),
        "max_continuations" => cfg.max_continuations.to_string(),
        "http_timeout" => cfg.req_timeout.to_string(),
        "shell_timeout" => cfg.shell_timeout.to_string(),
        "long_tool_timeout" => cfg.long_tool_timeout.to_string(),
        "max_tool_result_chars" => cfg.max_tool_result_chars.to_string(),
        "max_fetch_chars" => cfg.max_fetch_chars.to_string(),
        "geocode_countrycodes" => cfg.geocode_cc.clone(),
        "prefetch_recon" => cfg.prefetch_recon.to_string(),
        "summary" => cfg.summarize.to_string(),
        "verbose" => cfg.verbose.to_string(),
        "tor" => cfg.tor.to_string(),
        "tor_proxy" => cfg.tor_proxy.clone(),
        "case_db" => cfg.db_file.display().to_string(),
        "audit_log" => cfg.log_file.display().to_string(),
        _ => String::new(),
    }
}

fn gen_value(cfg: &Config, key: &str) -> String {
    match key {
        "num_ctx" => cfg.num_ctx.to_string(),
        "temperature" => cfg.gen_temp.to_string(),
        "top_p" => cfg.gen_top_p.to_string(),
        "repeat_penalty" => cfg.gen_repeat.to_string(),
        _ => cfg
            .model_options
            .get(key)
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default(),
    }
}

fn event_label(ev: &AgentEvent) -> (egui::Color32, String, String) {
    match ev {
        AgentEvent::Info(s) => (egui::Color32::GRAY, "· ".into(), s.clone()),
        AgentEvent::Thinking(s) => (egui::Color32::from_rgb(150, 130, 200), "🧠 ".into(), s.clone()),
        AgentEvent::ToolCall(s) => (egui::Color32::from_rgb(90, 170, 255), "→ ".into(), s.clone()),
        AgentEvent::ToolResult(s) => (egui::Color32::from_rgb(120, 120, 120), "  ".into(), s.clone()),
        AgentEvent::Perf(s) => (egui::Color32::from_rgb(120, 180, 120), "⚙ ".into(), s.clone()),
        AgentEvent::Hardware(s) => (egui::Color32::from_rgb(200, 160, 90), "⚙ ".into(), s.clone()),
        AgentEvent::Final(s) => (egui::Color32::WHITE, "".into(), s.clone()),
    }
}

fn passes(ev: &AgentEvent, v: Verbosity) -> bool {
    match v {
        Verbosity::Quiet => matches!(ev, AgentEvent::Final(_) | AgentEvent::Info(_)),
        Verbosity::Normal => !matches!(ev, AgentEvent::Thinking(_) | AgentEvent::ToolResult(_)),
        Verbosity::Verbose => true,
    }
}

fn verbosity_picker(ui: &mut egui::Ui, v: &mut Verbosity) {
    ui.horizontal(|ui| {
        ui.label("Verbosity:");
        ui.selectable_value(v, Verbosity::Quiet, "Quiet");
        ui.selectable_value(v, Verbosity::Normal, "Normal");
        ui.selectable_value(v, Verbosity::Verbose, "Verbose");
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("badbitch-rs");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Run, "▶ Run");
                ui.selectable_value(&mut self.tab, Tab::Settings, "⚙ Settings");
                ui.selectable_value(&mut self.tab, Tab::Prompt, "📝 Prompt");
                ui.separator();
                if ui.button("🧠 Thought process").clicked() {
                    self.show_thoughts = !self.show_thoughts;
                }
                ui.separator();
                let status = if self.running { "● running" } else { "idle" };
                ui.label(status);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Settings => self.settings_ui(ui),
            Tab::Prompt => self.prompt_ui(ui),
            Tab::Run => self.run_ui(ui),
        });

        // Thought-process window (toggleable "second window").
        let mut open = self.show_thoughts;
        egui::Window::new("🧠 Thought process & commands")
            .open(&mut open)
            .default_size([620.0, 620.0])
            .show(ctx, |ui| {
                verbosity_picker(ui, &mut self.thoughts_verbosity);
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                    for ev in &self.events {
                        if passes(ev, self.thoughts_verbosity) {
                            let (color, prefix, text) = event_label(ev);
                            ui.colored_label(color, format!("{prefix}{text}"));
                        }
                    }
                });
            });
        self.show_thoughts = open;

        if self.running {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
    }
}

impl App {
    fn run_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Target:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.target)
                    .desired_width(560.0)
                    .hint_text("address / name / domain / username / IP"),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("▶ Run").clicked() || enter) && !self.running {
                self.start_run();
            }
            if ui.button("Clear").clicked() {
                self.events.clear();
            }
        });
        verbosity_picker(ui, &mut self.run_verbosity);
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for ev in &self.events {
                    if !passes(ev, self.run_verbosity) {
                        continue;
                    }
                    let (color, prefix, text) = event_label(ev);
                    if matches!(ev, AgentEvent::Final(_)) {
                        ui.add_space(6.0);
                        ui.separator();
                    }
                    ui.colored_label(color, format!("{prefix}{text}"));
                }
            });
    }

    fn prompt_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("💾 Save override").clicked() {
                match badbitch::prompt::save_override(&self.prompt_text) {
                    Ok(()) => self.prompt_status = "Saved system-prompt override.".into(),
                    Err(e) => self.prompt_status = format!("Save failed: {e}"),
                }
            }
            if ui.button("↺ Reset to default").clicked() {
                let _ = badbitch::prompt::clear_override();
                self.prompt_text = badbitch::prompt::default_prompt().to_string();
                self.prompt_status = "Reset to built-in default.".into();
            }
            if !self.prompt_status.is_empty() {
                ui.label(&self.prompt_status);
            }
        });
        ui.label("System prompt (the model's playbook). Edits are saved to an override file; reset restores the built-in.");
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.prompt_text)
                    .desired_width(f32::INFINITY)
                    .desired_rows(30)
                    .code_editor(),
            );
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("💾 Save settings").clicked() {
                self.save_settings();
            }
            if !self.settings_status.is_empty() {
                ui.label(&self.settings_status);
            }
        });
        ui.label("Saved to ~/.config/badbitch-rs/config.ini. Restart a run (or the GUI) to apply.");
        ui.separator();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // Model
            ui.heading("Model");
            ui.horizontal(|ui| {
                ui.label("Model:").on_hover_text("Ollama model tag to run.");
                // Always include the currently-configured model so it can never go "missing"
                // (e.g. if Ollama was down at launch, or a fresh isolated config).
                let mut options = self.models.clone();
                if !self.model.is_empty() && !options.contains(&self.model) {
                    options.insert(0, self.model.clone());
                }
                egui::ComboBox::from_id_salt("model_combo")
                    .selected_text(if self.model.is_empty() { "(pick)".to_string() } else { self.model.clone() })
                    .width(420.0)
                    .show_ui(ui, |ui| {
                        for m in &options {
                            ui.selectable_value(&mut self.model, m.clone(), m);
                        }
                    });
                if ui.button("🔄 Refresh").on_hover_text("Re-query Ollama for installed models").clicked() {
                    self.models = fetch_models(&self.ollama_host);
                }
            });
            if self.models.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(210, 140, 90),
                    "No models from Ollama — is it running? Start it (or run badbitch-setup), then Refresh. You can also type a model tag below.",
                );
            }
            ui.horizontal(|ui| {
                ui.label("  or type:");
                ui.add(egui::TextEdit::singleline(&mut self.model).desired_width(420.0));
            });
            ui.horizontal(|ui| {
                ui.label("Ollama host:").on_hover_text("Where the Ollama server listens.");
                ui.add(egui::TextEdit::singleline(&mut self.ollama_host).desired_width(320.0));
            });
            ui.horizontal(|ui| {
                ui.label("Thinking:").on_hover_text("Reasoning models emit a thinking channel. Off = faster, no reasoning.");
                ui.selectable_value(&mut self.think, ThinkMode::Default, "Model default");
                ui.selectable_value(&mut self.think, ThinkMode::On, "On");
                ui.selectable_value(&mut self.think, ThinkMode::Off, "Off (no thinking)");
            });

            ui.add_space(8.0);
            ui.heading("Generation options (ollama)");
            egui::Grid::new("gen_grid").num_columns(2).striped(true).show(ui, |ui| {
                for (k, v) in self.gen_opts.iter_mut() {
                    ui.label(k.as_str()).on_hover_text(describe(k));
                    ui.add(egui::TextEdit::singleline(v).desired_width(300.0).hint_text(describe(k)));
                    ui.end_row();
                }
            });

            ui.add_space(8.0);
            ui.heading("Agent / OSINT");
            egui::Grid::new("osint_grid").num_columns(2).striped(true).show(ui, |ui| {
                for (k, v) in self.osint.iter_mut() {
                    ui.label(k.as_str()).on_hover_text(describe(k));
                    if BOOL_KEYS.contains(&k.as_str()) {
                        let mut b = v == "true";
                        if ui.checkbox(&mut b, "").changed() {
                            *v = b.to_string();
                        }
                    } else {
                        ui.add(egui::TextEdit::singleline(v).desired_width(360.0).hint_text(describe(k)));
                    }
                    ui.end_row();
                }
            });

            ui.add_space(8.0);
            ui.collapsing("API keys", |ui| {
                egui::Grid::new("keys_grid").num_columns(2).striped(true).show(ui, |ui| {
                    for (k, v) in self.keys.iter_mut() {
                        ui.label(k.as_str());
                        ui.add(egui::TextEdit::singleline(v).desired_width(360.0).password(true));
                        ui.end_row();
                    }
                });
            });
        });
    }
}
