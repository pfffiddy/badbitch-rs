//! badbitch-gui — native desktop control panel for badbitch-rs (egui/eframe).
//!
//! Three tabs — Settings, Prompt, Run — plus a toggleable "Thought process" window that
//! shows the model's reasoning, every command/tool it invokes, and per-turn perf/hardware,
//! with its own verbosity filter. Settings exposes every agent param, the model picker
//! (from your installed Ollama models), a thinking On/Off/Default toggle, and the full set
//! of Ollama generation options. Saving writes `~/.config/badbitch-rs/config.ini`.

use std::sync::mpsc::{Receiver, Sender, channel};

use badbitch::agent::{AgentEvent, RunControls};
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

// Ollama SERVER environment variables (applied by restarting the server, not per request).
const OLLAMA_ENV_KEYS: &[&str] = &[
    "OLLAMA_KV_CACHE_TYPE",
    "OLLAMA_FLASH_ATTENTION",
    "OLLAMA_KEEP_ALIVE",
    "OLLAMA_NUM_PARALLEL",
    "OLLAMA_MAX_LOADED_MODELS",
];

fn describe_env(key: &str) -> &'static str {
    match key {
        "OLLAMA_KV_CACHE_TYPE" => "KV cache quantization: q8_0 halves KV memory (fit more context on GPU) with tiny quality loss; q4_0 is smaller still; f16 = full precision.",
        "OLLAMA_FLASH_ATTENTION" => "1 = enable flash attention (less KV memory, faster). 0 = off.",
        "OLLAMA_KEEP_ALIVE" => "How long a model stays loaded when idle (e.g. 30m, 1h, or -1 to keep forever).",
        "OLLAMA_NUM_PARALLEL" => "Parallel requests per model (raises KV memory).",
        "OLLAMA_MAX_LOADED_MODELS" => "How many models can be resident at once.",
        _ => "",
    }
}

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
    oenv: Vec<(String, String)>,  // OLLAMA_ENV_KEYS order (server env vars)
    keys: Vec<(String, String)>,  // API_KEY_NAMES order
    models: Vec<String>,          // installed Ollama models
    settings_status: String,
    ollama_env_status: String,

    // ── prompt ──
    prompt_text: String,
    prompt_status: String,

    // ── run ──
    target: String,
    events: Vec<AgentEvent>,
    rx: Option<Receiver<AgentEvent>>,
    running: bool,
    run_verbosity: Verbosity,
    controls: Option<RunControls>, // live stop/inject handle for the current run
    truncate_display: usize,       // truncate each shown line to N chars (0 = off)

    // ── thought window ──
    show_thoughts: bool,
    thoughts_verbosity: Verbosity,
    inject_text: String, // the "inject a message mid-run" bar
}

/// Register broad-coverage system fonts as fallbacks so non-Latin text (Cyrillic, Greek,
/// CJK, Arabic, …) renders instead of "tofu" boxes. Uses whatever the OS already ships —
/// no bundled binary. Silently does nothing if none are found (egui defaults remain).
fn install_system_fonts(ctx: &egui::Context) {
    // Single-file ttf/otf only (egui/ab_glyph can't parse .ttc collections).
    let candidates = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", // Latin + Cyrillic + Greek (Kali/Debian)
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansArabic-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansHebrew-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansThai-Regular.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf", // Fedora/Arch layout
    ];
    let mut fonts = egui::FontDefinitions::default();
    let mut added: Vec<String> = Vec::new();
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let name = std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("sysfont")
                .to_string();
            fonts
                .font_data
                .insert(name.clone(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            added.push(name);
        }
    }
    if added.is_empty() {
        return;
    }
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for name in &added {
            list.push(name.clone()); // append as fallback (after the default fonts)
        }
    }
    ctx.set_fonts(fonts);
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_system_fonts(&cc.egui_ctx);
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
        let oenv = OLLAMA_ENV_KEYS
            .iter()
            .map(|k| ((*k).to_string(), cfg.ollama_env.get(*k).cloned().unwrap_or_default()))
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
            oenv,
            keys,
            models,
            settings_status: String::new(),
            ollama_env_status: String::new(),
            prompt_text: badbitch::prompt::base_prompt(),
            prompt_status: String::new(),
            target: String::new(),
            events: Vec::new(),
            rx: None,
            running: false,
            run_verbosity: Verbosity::Normal,
            controls: None,
            truncate_display: 0,
            show_thoughts: false,
            thoughts_verbosity: Verbosity::Verbose,
            inject_text: String::new(),
        }
    }

    /// Reload the editable settings + model list from disk (after a save, so the GUI's view
    /// matches what the next run will use — no restart needed).
    fn reload_settings(&mut self) {
        let cfg = Config::load();
        self.model = cfg.model.clone();
        self.ollama_host = cfg.ollama_host.clone();
        self.osint = OSINT_KEYS.iter().map(|k| ((*k).to_string(), osint_value(&cfg, k))).collect();
        self.gen_opts = GEN_KEYS.iter().map(|k| ((*k).to_string(), gen_value(&cfg, k))).collect();
        self.oenv = OLLAMA_ENV_KEYS
            .iter()
            .map(|k| ((*k).to_string(), cfg.ollama_env.get(*k).cloned().unwrap_or_default()))
            .collect();
        self.keys = API_KEY_NAMES.iter().map(|k| ((*k).to_string(), cfg.key(k))).collect();
        self.think = match cfg.think {
            None => ThinkMode::Default,
            Some(true) => ThinkMode::On,
            Some(false) => ThinkMode::Off,
        };
        self.models = fetch_models(&cfg.ollama_host);
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

        // Ollama server env vars — remembered so the GUI can re-show them and re-apply.
        let oenv_kv: Vec<(String, String)> = self
            .oenv
            .iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .map(|(k, v)| (k.clone(), v.trim().to_string()))
            .collect();

        let sections: Vec<(&str, Vec<(String, String)>)> = vec![
            ("model", vec![("name".to_string(), self.model.clone())]),
            ("osint", osint_kv),
            ("model_options", mopts),
            ("ollama_env", oenv_kv),
            ("api_keys", keys_kv),
        ];

        match write_ini(&Config::config_path(), &sections) {
            Ok(()) => {
                self.settings_status = format!("Saved to {}", Config::config_path().display());
            }
            Err(e) => self.settings_status = format!("Save failed: {e}"),
        }
    }

    /// Non-empty (KEY, VALUE) env pairs currently entered.
    fn oenv_pairs(&self) -> Vec<(String, String)> {
        self.oenv
            .iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .map(|(k, v)| (k.clone(), v.trim().to_string()))
            .collect()
    }

    /// The shell commands that write a systemd drop-in and restart Ollama, for the "Copy"
    /// fallback (users whose Ollama isn't a systemd service can adapt these).
    fn restart_commands(&self) -> String {
        let pairs = self.oenv_pairs();
        if pairs.is_empty() {
            return "# No Ollama server env vars set.".to_string();
        }
        let lines: String = pairs
            .iter()
            .map(|(k, v)| format!("Environment=\"{k}={v}\"\n"))
            .collect();
        format!(
            "sudo mkdir -p /etc/systemd/system/ollama.service.d\n\
             sudo tee /etc/systemd/system/ollama.service.d/badbitch.conf >/dev/null <<'EOF'\n\
             [Service]\n{lines}EOF\n\
             sudo systemctl daemon-reload\n\
             sudo systemctl restart ollama"
        )
    }

    /// Write the systemd drop-in and restart Ollama via pkexec (graphical admin prompt).
    /// These are SERVER env vars — they only take effect after the server restarts.
    fn apply_ollama_env(&mut self) {
        let pairs = self.oenv_pairs();
        if pairs.is_empty() {
            self.ollama_env_status = "Nothing to apply (no env vars set).".into();
            return;
        }
        let lines: String = pairs
            .iter()
            .map(|(k, v)| format!("Environment=\"{k}={v}\"\n"))
            .collect();
        let script = format!(
            "set -e\n\
             mkdir -p /etc/systemd/system/ollama.service.d\n\
             cat > /etc/systemd/system/ollama.service.d/badbitch.conf <<'EOF'\n\
             [Service]\n{lines}EOF\n\
             systemctl daemon-reload\n\
             systemctl restart ollama"
        );
        match std::process::Command::new("pkexec").arg("sh").arg("-c").arg(&script).status() {
            Ok(s) if s.success() => {
                self.ollama_env_status = "Applied — Ollama restarted with the new server settings.".into();
            }
            Ok(s) => {
                self.ollama_env_status =
                    format!("pkexec exited with {s} — cancelled, or Ollama isn't a systemd service. Use Copy commands instead.");
            }
            Err(e) => {
                self.ollama_env_status = format!("Couldn't launch pkexec ({e}). Use Copy commands instead.");
            }
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
        let controls = RunControls::new();
        self.controls = Some(controls.clone());

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
                    Some(&controls),
                )
                .await;
                let _ = tx.send(AgentEvent::Final(ans));
            });
        });
    }

    /// Ask the running turn to stop (Esc / Stop button).
    fn stop_run(&mut self) {
        if let Some(c) = &self.controls {
            c.stop();
        }
    }

    /// Send a message: inject into the running turn, or start a fresh run if idle.
    fn send_message(&mut self, msg: String) {
        let msg = msg.trim().to_string();
        if msg.is_empty() {
            return;
        }
        if self.running {
            if let Some(c) = &self.controls {
                c.inject(msg);
            }
        } else {
            self.target = msg;
            self.start_run();
        }
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

/// Sanitize (strip control/binary bytes) then truncate a displayed line to `n` chars
/// (0 = no truncation). Keeps stray binary from a fetch/shell tool out of the UI.
fn clip_line(s: &str, n: usize) -> String {
    let s = badbitch::http::sanitize_text(s);
    if n == 0 || s.chars().count() <= n {
        s
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        // Esc stops a running turn (like Claude Code) and lets you type again.
        if self.running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.stop_run();
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("badbitch-rs");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Run, "▶ Run");
                ui.selectable_value(&mut self.tab, Tab::Settings, "⚙ Settings");
                ui.selectable_value(&mut self.tab, Tab::Prompt, "📝 Prompt");
                ui.separator();
                let tlabel = if self.show_thoughts { "🧠 Thought window: on" } else { "🧠 Thought window" };
                if ui.button(tlabel).clicked() {
                    self.show_thoughts = !self.show_thoughts;
                }
                ui.separator();
                ui.label(if self.running { "● running" } else { "idle" });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Settings => self.settings_ui(ui),
            Tab::Prompt => self.prompt_ui(ui),
            Tab::Run => self.run_ui(ui),
        });

        // Thought process — a REAL separate OS window (immediate viewport), movable/closable,
        // with its own inject bar, Stop, and verbosity.
        if self.show_thoughts {
            let vid = egui::ViewportId::from_hash_of("badbitch-thoughts");
            let builder = egui::ViewportBuilder::default()
                .with_title("badbitch-rs — Thought process")
                .with_inner_size([700.0, 740.0]);
            ctx.show_viewport_immediate(vid, builder, |vctx, _class| {
                egui::TopBottomPanel::top("inject_bar").show(vctx, |ui| self.thoughts_bar(ui));
                egui::CentralPanel::default().show(vctx, |ui| self.thoughts_body(ui));
                if vctx.input(|i| i.viewport().close_requested()) {
                    self.show_thoughts = false;
                }
                if self.running {
                    vctx.request_repaint_after(std::time::Duration::from_millis(120));
                }
            });
        }

        if self.running {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
    }
}

impl App {
    /// Top bar of the thought window: inject/run box, Stop, verbosity. This is the "talk to
    /// it while running" bar the user asked for.
    fn thoughts_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let hint = if self.running {
                "inject a note / redirect the agent, then Enter…"
            } else {
                "type a target and Enter to start a run…"
            };
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.inject_text)
                    .desired_width(430.0)
                    .hint_text(hint),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let label = if self.running { "Inject" } else { "Run" };
            if ui.button(label).clicked() || enter {
                let m = std::mem::take(&mut self.inject_text);
                self.send_message(m);
            }
            if self.running && ui.button("⏹ Stop").clicked() {
                self.stop_run();
            }
            if ui.button("📋 Copy all").clicked() {
                ui.ctx().copy_text(self.transcript(self.thoughts_verbosity));
            }
        });
        verbosity_picker(ui, &mut self.thoughts_verbosity);
    }

    /// Build the filtered transcript as one string (for a single selectable text area).
    fn transcript(&self, v: Verbosity) -> String {
        self.events
            .iter()
            .filter(|e| passes(e, v))
            .map(|e| {
                let (_c, prefix, text) = event_label(e);
                format!("{prefix}{}", clip_line(&text, self.truncate_display))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Render the transcript in ONE read-only-ish multiline text area, so the whole thing is
    /// selectable (drag, Ctrl+A) and copyable (Ctrl+C). Auto-tails only while running, so a
    /// finished run stays put while you select. Edits are discarded (regenerated each frame).
    fn transcript_view(&self, ui: &mut egui::Ui, v: Verbosity) {
        let mut text = self.transcript(v);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(self.running)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(24)
                        .font(egui::TextStyle::Monospace),
                );
            });
    }

    /// Scrolling body of the thought window: reasoning + commands + events (selectable).
    fn thoughts_body(&mut self, ui: &mut egui::Ui) {
        self.transcript_view(ui, self.thoughts_verbosity);
    }

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
            if self.running && ui.button("⏹ Stop").clicked() {
                self.stop_run();
            }
            if ui.button("Clear").clicked() {
                self.events.clear();
            }
        });
        ui.horizontal(|ui| {
            verbosity_picker(ui, &mut self.run_verbosity);
            ui.separator();
            if ui.button("📋 Copy all").clicked() {
                ui.ctx().copy_text(self.transcript(self.run_verbosity));
            }
        });
        ui.separator();
        self.transcript_view(ui, self.run_verbosity);
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
                self.reload_settings(); // refresh the view so it matches what the next run uses
            }
            if !self.settings_status.is_empty() {
                ui.label(&self.settings_status);
            }
        });
        ui.label("Saved to ~/.config/badbitch-rs/config.ini and applied on the NEXT run — no restart needed (each run reloads the config).");
        ui.horizontal(|ui| {
            ui.label("Truncate displayed lines to:").on_hover_text("Shorten long lines in the Run/Thought views. 0 = show full text.");
            ui.add(egui::DragValue::new(&mut self.truncate_display).range(0..=100_000).suffix(" chars"));
        });
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
            ui.heading("Ollama server (KV cache / flash attention)");
            ui.label("These are SERVER env vars — they take effect only after Ollama restarts, not per run. Set q8_0 KV cache + flash attention to fit more context on the GPU.");
            egui::Grid::new("oenv_grid").num_columns(2).striped(true).show(ui, |ui| {
                for (k, v) in self.oenv.iter_mut() {
                    ui.label(k.as_str()).on_hover_text(describe_env(k));
                    match k.as_str() {
                        "OLLAMA_KV_CACHE_TYPE" => {
                            egui::ComboBox::from_id_salt("kv_cache_combo")
                                .selected_text(if v.is_empty() { "(default / f16)".to_string() } else { v.clone() })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(v, String::new(), "(default / f16)");
                                    ui.selectable_value(v, "q8_0".to_string(), "q8_0 (half KV, recommended)");
                                    ui.selectable_value(v, "q4_0".to_string(), "q4_0 (smallest)");
                                    ui.selectable_value(v, "f16".to_string(), "f16 (full precision)");
                                });
                        }
                        "OLLAMA_FLASH_ATTENTION" => {
                            egui::ComboBox::from_id_salt("flash_attn_combo")
                                .selected_text(if v.is_empty() { "(default)".to_string() } else { v.clone() })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(v, String::new(), "(default)");
                                    ui.selectable_value(v, "1".to_string(), "1 (on, recommended)");
                                    ui.selectable_value(v, "0".to_string(), "0 (off)");
                                });
                        }
                        _ => {
                            ui.add(egui::TextEdit::singleline(v).desired_width(300.0).hint_text(describe_env(k)));
                        }
                    }
                    ui.end_row();
                }
            });
            ui.horizontal(|ui| {
                if ui.button("⚡ Apply & restart Ollama (admin)").on_hover_text("Writes a systemd drop-in and restarts Ollama via pkexec.").clicked() {
                    self.apply_ollama_env();
                }
                if ui.button("📋 Copy restart commands").on_hover_text("Copy the shell commands to apply these manually.").clicked() {
                    ui.ctx().copy_text(self.restart_commands());
                }
            });
            if !self.ollama_env_status.is_empty() {
                ui.label(&self.ollama_env_status);
            }

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
