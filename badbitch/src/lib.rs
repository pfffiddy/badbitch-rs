//! badbitch-rs library surface — shared by the `badbitch` CLI and the `badbitch-gui` app.

#![allow(dead_code)]

pub mod agent;
pub mod classify;
pub mod compact;
pub mod config;
pub mod debug;
pub mod http;
pub mod ollama;
pub mod prompt;
pub mod recovery_calls;
pub mod shell;
pub mod store;
pub mod tool;
pub mod util;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use crate::config::Config;
use crate::tool::ToolContext;

/// Template written by `--init-config` and by the GUI when no config exists yet.
pub const CONFIG_TEMPLATE: &str = r#"[model]
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
verbose               = false
summary               = true
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

/// Build the shared tool-execution context (HTTP client with optional Tor, docs scratch dir,
/// case-DB path). Used by both the CLI and the GUI so runs behave identically.
pub fn build_context(cfg: &Arc<Config>) -> ToolContext {
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
    let _ = std::fs::remove_dir_all(&docs_dir);

    ToolContext {
        config: cfg.clone(),
        http,
        docs_dir,
        doc_seq: Arc::new(AtomicUsize::new(0)),
        db_path: cfg.db_file.clone(),
    }
}

/// The per-run debug log path (cwd, truncated each session).
pub fn debug_log_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("badbitch-rs_debug.log")
}
