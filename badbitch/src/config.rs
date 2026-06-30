//! Config loader — ports the `CFG.*` reads from badbitch2.py:71-104.
//!
//! INI only, from `~/.config/badbitch/config.ini`. Every value falls back to the same
//! default as the Python. Per the port's "separate DB" decision the case store is a
//! sibling `*_rs.sqlite` file so the Python tool's saved cases are never touched.

use std::collections::HashMap;
use std::path::PathBuf;

use configparser::ini::Ini;

pub const UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";

#[derive(Debug, Clone)]
pub struct Config {
    pub config_path: PathBuf,
    pub model: String,
    pub ollama_host: String,
    pub searx_url: String,
    pub num_ctx: i64,
    pub max_tool_iters: usize,
    pub max_continuations: usize,
    pub compact_threshold: usize,
    pub compact_keep: usize,
    pub req_timeout: u64,
    pub shell_timeout: u64,
    pub long_tool_timeout: u64,
    pub max_tool_result_chars: usize,
    pub max_fetch_chars: usize,
    pub gen_temp: f64,
    pub gen_top_p: f64,
    pub gen_repeat: f64,
    pub prefetch_recon: bool,
    pub geocode_cc: String,
    pub tor: bool,
    pub tor_proxy: String,
    pub db_file: PathBuf,
    pub log_file: PathBuf,
    pub api_keys: HashMap<String, String>,
    /// [osint] verbose = true — surface per-tool timing, retry notices, rate-limit waits.
    pub verbose: bool,
    /// [osint] summary = true — generate a 3-5 bullet TL;DR after each turn.
    pub summarize: bool,
    /// UA rotation pool for per-request header variation (badbitch2.py:129).
    pub ua_pool: Vec<String>,
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// Derive the Rust case-DB path from the Python `case_db`: insert `_rs` before the
/// extension so `osint_cases.sqlite` → `osint_cases_rs.sqlite`.
fn rs_db_path(case_db: &str) -> PathBuf {
    let p = expand(case_db);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("osint_cases");
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("sqlite");
    let parent = p.parent().map(|x| x.to_path_buf()).unwrap_or_default();
    parent.join(format!("{stem}_rs.{ext}"))
}

impl Config {
    pub fn config_path() -> PathBuf {
        home().join(".config/badbitch/config.ini")
    }

    /// Load config, applying Python's fallbacks for any missing key. A missing file is fine
    /// (every value defaults), matching `CFG.read()` over a nonexistent path.
    pub fn load() -> Self {
        let path = Self::config_path();
        let mut ini = Ini::new();
        let _ = ini.load(&path); // ignore "file not found" — defaults cover everything

        let s = |sec: &str, k: &str| ini.get(sec, k).map(|v| v.trim().to_string());
        let osint = |k: &str| s("osint", k);
        let geti = |k: &str, d: i64| {
            osint(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(d)
        };
        let getf = |k: &str, d: f64| {
            osint(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(d)
        };
        let getb = |k: &str, d: bool| match osint(k).map(|v| v.to_lowercase()) {
            Some(v) => matches!(v.as_str(), "true" | "yes" | "1" | "on"),
            None => d,
        };

        let case_db = osint("case_db")
            .unwrap_or_else(|| "~/.local/share/badbitch/osint_cases.sqlite".into());
        let log_file = osint("audit_log")
            .unwrap_or_else(|| "~/.local/share/badbitch/osint_audit.log".into());

        // API keys: every entry under [api_keys].
        let mut api_keys = HashMap::new();
        if let Some(map) = ini.get_map()
            && let Some(keys) = map.get("api_keys")
        {
            for (k, v) in keys {
                if let Some(v) = v {
                    api_keys.insert(k.clone(), v.trim().to_string());
                }
            }
        }

        let ollama_host = std::env::var("OLLAMA_HOST")
            .ok()
            .map(|h| normalize_host(&h))
            .or_else(|| osint("ollama_host").map(|h| normalize_host(&h)))
            .unwrap_or_else(|| "http://127.0.0.1:11434".into());

        Config {
            config_path: path,
            model: s("model", "name")
                .unwrap_or_else(|| "hf.co/unsloth/Qwen3-14B-GGUF:IQ4_XS".into()),
            ollama_host,
            searx_url: osint("searxng_url")
                .unwrap_or_else(|| "http://127.0.0.1:8888/search".into()),
            num_ctx: geti("num_ctx", 20480),
            max_tool_iters: geti("max_tool_iters", 40) as usize,
            max_continuations: geti("max_continuations", 6) as usize,
            compact_threshold: geti("compact_threshold", 40) as usize,
            compact_keep: geti("compact_keep", 20) as usize,
            req_timeout: geti("http_timeout", 15) as u64,
            shell_timeout: geti("shell_timeout", 120) as u64,
            long_tool_timeout: geti("long_tool_timeout", 300) as u64,
            max_tool_result_chars: geti("max_tool_result_chars", 4000) as usize,
            max_fetch_chars: geti("max_fetch_chars", 8000) as usize,
            gen_temp: getf("temperature", 0.3),
            gen_top_p: getf("top_p", 0.9),
            gen_repeat: getf("repeat_penalty", 1.1),
            prefetch_recon: getb("prefetch_recon", true),
            geocode_cc: osint("geocode_countrycodes").unwrap_or_else(|| "us".into()),
            tor: getb("tor", false),
            tor_proxy: osint("tor_proxy")
                .unwrap_or_else(|| "socks5h://127.0.0.1:9050".into()),
            db_file: rs_db_path(&case_db),
            log_file: expand(&log_file),
            api_keys,
            verbose: getb("verbose", false),
            summarize: getb("summary", true),
            ua_pool: vec![
                UA.to_string(),
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".into(),
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.1 Safari/605.1.15".into(),
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:121.0) Gecko/20100101 Firefox/121.0".into(),
            ],
        }
    }

    /// Pick a random UA from the pool for per-request rotation (badbitch2.py:137 `_pick_ua`).
    pub fn pick_ua(&self) -> &str {
        use std::time::{SystemTime, UNIX_EPOCH};
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos() as usize;
        &self.ua_pool[t % self.ua_pool.len()]
    }

    /// `_key()` (badbitch2.py:153): read an API key, returning "" if absent.
    pub fn key(&self, name: &str) -> String {
        self.api_keys.get(name).cloned().unwrap_or_default()
    }

    /// `_need_key()` (badbitch2.py:158): a clear "fill this config slot" message.
    pub fn need_key(&self, name: &str, friendly: &str, signup: &str) -> String {
        format!(
            "[{friendly}: no API key] add it to {} under:\n  [api_keys]\n  {name} = <your key>\nGet one: {signup}",
            self.config_path.display()
        )
    }
}

fn normalize_host(h: &str) -> String {
    let h = h.trim();
    if h.starts_with("http://") || h.starts_with("https://") {
        h.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", h.trim_end_matches('/'))
    }
}
