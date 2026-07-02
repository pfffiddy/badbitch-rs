//! badbitch-rs — full-spectrum OSINT agent (Rust port of badbitch2.py), driving the same
//! local abliterated model through Ollama.

// Phase-1 deliberately ships some support code the Phase-2 tools will consume (API-key
// helpers, fetch_json, run_shell_line, extra config fields). Allow it to keep the build quiet
// until those tools land.
#![allow(dead_code)]

mod agent;
mod classify;
mod compact;
mod config;
mod debug;
mod http;
mod ollama;
mod prompt;
mod recovery_calls;
mod shell;
mod store;
mod tool;
mod util;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use clap::Parser;

use crate::config::Config;
use crate::ollama::{ChatMessage, OllamaClient};
use crate::tool::{ToolContext, toolset};

#[derive(Parser, Debug)]
#[command(name = "badbitch", about = "Full-spectrum OSINT agent (Rust).")]
struct Cli {
    /// Single-shot query; omit for the interactive REPL.
    query: Vec<String>,
    /// Show tools + key/CLI availability.
    #[arg(long)]
    list_tools: bool,
    /// List saved cases.
    #[arg(long)]
    list_cases: bool,
    /// Print a saved dossier.
    #[arg(long, value_name = "ID")]
    show_case: Option<String>,
    /// Export a saved dossier to .md.
    #[arg(long, value_name = "ID")]
    export: Option<String>,
    /// Output path for --export.
    #[arg(long, value_name = "PATH")]
    out: Option<String>,
    /// Write a template config.ini with all key slots.
    #[arg(long)]
    init_config: bool,
    /// Surface per-tool timing, retry notices, and rate-limit waits.
    #[arg(short, long)]
    verbose: bool,
}

fn build_context(cfg: &Arc<Config>) -> ToolContext {
    let mut builder = reqwest::Client::builder();
    if cfg.tor {
        match reqwest::Proxy::all(&cfg.tor_proxy) {
            Ok(p) => builder = builder.proxy(p),
            Err(e) => eprintln!("[warn] bad tor_proxy {}: {e} — continuing without Tor", cfg.tor_proxy),
        }
    }
    let http = builder.build().unwrap_or_else(|_| reqwest::Client::new());

    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let docs_dir = workdir.join("case_docs");
    let _ = std::fs::remove_dir_all(&docs_dir); // _reset_docs (badbitch2.py:125)

    ToolContext {
        config: cfg.clone(),
        http,
        docs_dir,
        doc_seq: Arc::new(AtomicUsize::new(0)),
        db_path: cfg.db_file.clone(),
    }
}

fn debug_log_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("badbitch-rs_debug.log")
}

async fn print_list_tools(cfg: &Config) {
    let router = toolset();
    let mut names: Vec<String> = router.names().iter().map(|s| s.to_string()).collect();
    names.sort();
    println!("badbitch OSINT — {} tools  (model={})\n", names.len(), cfg.model);
    // Map tool name -> required API key name
    let key_tools = [
        ("shodan", "shodan"),
        ("censys", "censys_id"),
        ("dnsdumpster", "dnsdumpster"),
        ("virustotal", "virustotal"),
        ("intelx", "intelx"),
        ("rocketreach", "rocketreach"),
        ("dehashed", "dehashed_key"),
        ("breach_check", "hibp"),
        ("attom_property", "attom"),
        ("regrid_parcel", "regrid"),
    ];
    for n in names {
        let status: String = if let Some((_, key)) = key_tools.iter().find(|(t, _)| *t == n.as_str()) {
            if cfg.key(key).is_empty() { format!("✗ no key ({key})") } else { "✓ key".to_string() }
        } else {
            match n.as_str() {
            "sherlock" => {
                if shell::have("sherlock").await { "✓ sherlock".into() } else { "✗ missing sherlock".into() }
            }
            "holehe" => {
                if shell::have("holehe").await { "✓ holehe".into() } else { "✗ missing holehe".into() }
            }
            "theharvester" => {
                if shell::have("theHarvester").await { "✓ theHarvester".into() } else { "✗ missing theHarvester".into() }
            }
            "phoneinfoga" => {
                if shell::have("phoneinfoga").await { "✓ phoneinfoga".into() } else { "✗ missing phoneinfoga".into() }
            }
            "exif_metadata" => {
                if shell::have("exiftool").await { "✓ exiftool".into() } else { "✗ missing exiftool".into() }
            }
            "run_shell" | "python_eval" => "✓ (shell)".into(),
            "fetch_rendered" => {
                if shell::have("python3").await { "✓ python3 (needs playwright)".into() } else { "✗ missing python3".into() }
            }
            _ => "✓".to_string(),
        }};
        println!("  {n:<22} {status}");
    }
}

const CONFIG_TEMPLATE: &str = r#"[model]
name = richardyoung/qwen3-14b-abliterated:latest

[osint]
searxng_url       = http://127.0.0.1:8888/search
num_ctx           = 20480
max_tool_iters    = 40
max_continuations = 12
http_timeout      = 30
shell_timeout     = 1120
long_tool_timeout = 300
max_tool_result_chars = 4000
max_fetch_chars       = 8000
temperature           = 0.3
top_p                 = 0.9
repeat_penalty        = 1.1
prefetch_recon        = true
geocode_countrycodes  = us
tor       = false
tor_proxy = socks5h://127.0.0.1:9050
# ollama_host = http://127.0.0.1:11434
case_db   = ~/.local/share/badbitch-rs/osint_cases.sqlite
audit_log = ~/.local/share/badbitch-rs/osint_audit.log

[api_keys]
shodan         =
censys_id      =
censys_secret  =
virustotal     =
intelx         =
intelx_base    = https://2.intelx.io
dnsdumpster    =
rocketreach    =
opencorporates =
attom          =
regrid         =
hibp           =
dehashed_email =
dehashed_key   =
"#;

async fn single_shot(cfg: Arc<Config>, query: String) {
    debug::init(&debug_log_path());
    let ctx = build_context(&cfg);
    let client = OllamaClient::new(&cfg);
    let router = toolset();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let mut messages = vec![ChatMessage::system(prompt::system_prompt(&cfg, &workdir))];
    agent::preflight(&ctx, &cfg, &mut messages, &query).await;
    messages.push(ChatMessage::user(query));
    let answer = agent::run_turn(&client, &router, &ctx, &cfg, &mut messages).await;
    println!("\n{answer}\n");
}

async fn repl(cfg: Arc<Config>) {
    println!("BadBitch OSINT  |  model={}  searxng={}", cfg.model, cfg.searx_url);
    let router = toolset();
    println!(
        "{} tools. type a target (address / name / domain / username / IP). /reset clears history, exit/quit to leave.\n",
        router.names().len()
    );
    debug::init(&debug_log_path());
    println!("(debug log: {})\n", debug_log_path().display());
    if cfg.num_ctx >= 24576 {
        println!(
            "[warn] num_ctx={} — on a 12 GB GPU this can spill to RAM/OOM and degrade output. 20480 recommended.\n",
            cfg.num_ctx
        );
    }

    let ctx = build_context(&cfg);
    let client = OllamaClient::new(&cfg);
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let fresh = || vec![ChatMessage::system(prompt::system_prompt(&cfg, &workdir))];
    let mut messages = fresh();

    let mut rl = match rustyline::DefaultEditor::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[fatal] could not start REPL: {e}");
            return;
        }
    };
    loop {
        let line = match rl.readline("osint > ") {
            Ok(l) => l.trim().to_string(),
            Err(_) => {
                println!();
                break;
            }
        };
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line.as_str());
        let low = line.to_lowercase();
        if matches!(low.as_str(), "exit" | "quit") {
            break;
        }
        if matches!(low.as_str(), "/reset" | "reset" | "/clear" | "clear" | "/new" | "new") {
            messages = fresh();
            // reset collected docs + counter
            let _ = std::fs::remove_dir_all(&ctx.docs_dir);
            ctx.doc_seq.store(0, std::sync::atomic::Ordering::SeqCst);
            println!("\n[history + collected docs cleared — fresh case]\n");
            continue;
        }
        agent::preflight(&ctx, &cfg, &mut messages, &line).await;
        messages.push(ChatMessage::user(line));
        let answer = agent::run_turn(&client, &router, &ctx, &cfg, &mut messages).await;
        println!("\n{answer}\n");
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let mut cfg_inner = Config::load();
    if cli.verbose {
        cfg_inner.verbose = true;
    }
    let cfg = Arc::new(cfg_inner);

    if cli.init_config {
        let path = Config::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if path.exists() {
            println!("[exists] {} — not overwriting. Remove it first to regenerate.", path.display());
        } else {
            match std::fs::write(&path, CONFIG_TEMPLATE) {
                Ok(()) => println!("[written] {} — fill in [api_keys].", path.display()),
                Err(e) => eprintln!("[init-config error] {e}"),
            }
        }
        return;
    }
    if cli.list_tools {
        print_list_tools(&cfg).await;
        return;
    }
    if cli.list_cases {
        println!("{}", store::list_cases(&cfg.db_file));
        return;
    }
    if let Some(id) = cli.show_case {
        println!("{}", store::load_dossier(&cfg.db_file, &id));
        return;
    }
    if let Some(id) = cli.export {
        let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        println!(
            "{}",
            store::export_dossier(&cfg.db_file, &id, cli.out.as_deref().unwrap_or(""), &workdir)
        );
        return;
    }

    if !cli.query.is_empty() {
        single_shot(cfg, cli.query.join(" ")).await;
    } else {
        repl(cfg).await;
    }
}
