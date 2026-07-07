//! Config loader — ports the `CFG.*` reads from badbitch2.py:71-104.
//!
//! INI only, from `~/.config/badbitch-rs/config.ini`. Every value falls back to a sane
//! default, so a fresh install with no config file just works. badbitch-rs is fully
//! self-contained: its config, case DB, and audit log all live under a dedicated
//! `badbitch-rs` namespace and never read or write another tool's files.

use std::collections::HashMap;
use std::path::PathBuf;

use configparser::ini::Ini;
use serde_json::Value;

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
    /// [osint] think — None = model default; Some(false) disables a reasoning model's
    /// thinking channel (faster); Some(true) forces it on.
    pub think: Option<bool>,
    /// [model_options] — arbitrary Ollama generation options (temperature, top_k, num_gpu,
    /// mirostat, …) passed straight through in the chat request's `options`.
    pub model_options: serde_json::Map<String, serde_json::Value>,
    /// [ollama_env] — remembered Ollama SERVER env vars (OLLAMA_KV_CACHE_TYPE,
    /// OLLAMA_FLASH_ATTENTION, …). These aren't sent per-request; the GUI applies them by
    /// restarting the Ollama server. Stored here only so the GUI remembers them.
    pub ollama_env: std::collections::BTreeMap<String, String>,
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

impl Config {
    pub fn config_path() -> PathBuf {
        home().join(".config/badbitch-rs/config.ini")
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
            .unwrap_or_else(|| "~/.local/share/badbitch-rs/osint_cases.sqlite".into());
        let log_file = osint("audit_log")
            .unwrap_or_else(|| "~/.local/share/badbitch-rs/osint_audit.log".into());

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

        // [osint] think = true/false — absent leaves the model default.
        let think = osint("think").map(|v| matches!(v.to_lowercase().as_str(), "true" | "yes" | "1" | "on"));

        // [model_options] — pass-through Ollama options, typed best-effort.
        let mut model_options = serde_json::Map::new();
        if let Some(map) = ini.get_map()
            && let Some(sec) = map.get("model_options")
        {
            for (k, v) in sec {
                if let Some(v) = v {
                    let v = v.trim();
                    if !v.is_empty() {
                        model_options.insert(k.clone(), parse_opt_value(v));
                    }
                }
            }
        }

        // [ollama_env] — remembered server env vars (strings, upper-cased keys).
        let mut ollama_env = std::collections::BTreeMap::new();
        if let Some(map) = ini.get_map()
            && let Some(sec) = map.get("ollama_env")
        {
            for (k, v) in sec {
                if let Some(v) = v {
                    let v = v.trim();
                    if !v.is_empty() {
                        ollama_env.insert(k.to_uppercase(), v.to_string());
                    }
                }
            }
        }

        Config {
            config_path: path,
            // A 14B abliterated model — fits a 12 GB GPU ~100% (no CPU offload); matches the
            // config template and badbitch-setup default. Bigger models need more VRAM.
            model: s("model", "name")
                .unwrap_or_else(|| "richardyoung/qwen3-14b-abliterated:latest".into()),
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
            db_file: expand(&case_db),
            log_file: expand(&log_file),
            api_keys,
            verbose: getb("verbose", false),
            summarize: getb("summary", true),
            think,
            model_options,
            ollama_env,
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

/// Best-effort typing of a config option value for Ollama: int, float, bool, comma-list
/// (for `stop`), else string.
fn parse_opt_value(s: &str) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        return Value::from(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::from(f);
    }
    match s.to_lowercase().as_str() {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if s.contains(',') {
        return Value::Array(s.split(',').map(|x| Value::from(x.trim())).collect());
    }
    Value::from(s)
}

fn normalize_host(h: &str) -> String {
    let h = h.trim();
    if h.starts_with("http://") || h.starts_with("https://") {
        h.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", h.trim_end_matches('/'))
    }
}

/// Ordered `[api_keys]` slots, so the GUI can render them in a stable order.
pub const API_KEY_NAMES: &[&str] = &[
    "shodan",
    "censys_id",
    "censys_secret",
    "virustotal",
    "intelx",
    "intelx_base",
    "dnsdumpster",
    "rocketreach",
    "opencorporates",
    "attom",
    "regrid",
    "hibp",
    "dehashed_email",
    "dehashed_key",
];

/// Write an INI file from ordered sections. Used by the GUI's "Save settings" so the config
/// stays human-readable and in a predictable order. Creates the parent dir.
pub fn write_ini(
    path: &std::path::Path,
    sections: &[(&str, Vec<(String, String)>)],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for (sec, kvs) in sections {
        out.push_str(&format!("[{sec}]\n"));
        for (k, v) in kvs {
            out.push_str(&format!("{k} = {v}\n"));
        }
        out.push('\n');
    }
    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_value_typing() {
        assert_eq!(parse_opt_value("42"), Value::from(42));
        assert_eq!(parse_opt_value("0.7"), Value::from(0.7));
        assert_eq!(parse_opt_value("true"), Value::Bool(true));
        assert_eq!(parse_opt_value("false"), Value::Bool(false));
        assert_eq!(parse_opt_value("a, b"), Value::Array(vec![Value::from("a"), Value::from("b")]));
        assert_eq!(parse_opt_value("qwen3"), Value::from("qwen3"));
    }

    #[test]
    fn host_normalization() {
        assert_eq!(normalize_host("127.0.0.1:11434"), "http://127.0.0.1:11434");
        assert_eq!(normalize_host("http://x:11434/"), "http://x:11434");
        assert_eq!(normalize_host("https://h/"), "https://h");
    }

    #[test]
    fn expand_tilde() {
        let p = expand("~/foo");
        assert!(p.ends_with("foo"));
        assert!(!p.to_string_lossy().starts_with('~'));
        assert_eq!(expand("/abs/path"), std::path::PathBuf::from("/abs/path"));
    }
}
