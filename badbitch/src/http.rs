//! Shared HTTP layer — ports `_http`/`_get` retry/backoff (badbitch2.py:188), `_rate_limit`
//! (178), HTML `_extract` (361), `fetch_url`/`_fetch_url_full` (377/404), `fetch_json` (445).
//!
//! Tor routing (`_proxies`, 172) is applied once at client-build time in `main` (reqwest sets
//! proxies per-client, not per-request), so there's nothing to thread through here.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use crate::config::{Config, UA};
use crate::util::clip_marked;

static LAST_CALL: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `_rate_limit` (badbitch2.py:178): block until `min_interval` has passed since the last
/// call tagged `bucket` (e.g. Nominatim 1/s).
pub async fn rate_limit(bucket: &str, min_interval: f64) {
    let wait = {
        let mut map = LAST_CALL.lock().unwrap();
        let now = Instant::now();
        let elapsed = map.get(bucket).map(|t| now.duration_since(*t).as_secs_f64());
        let wait = match elapsed {
            Some(e) => (min_interval - e).max(0.0),
            None => 0.0,
        };
        if wait <= 0.0 {
            map.insert(bucket.to_string(), now);
        }
        wait
    };
    if wait > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        LAST_CALL
            .lock()
            .unwrap()
            .insert(bucket.to_string(), Instant::now());
    }
}

/// General request with retry/backoff on transient failures (timeouts, 429, 5xx), mirroring
/// `_http` (badbitch2.py:188): up to `retries` extra attempts, backoff `1.5**(attempt+1)`.
/// Supports query, headers, optional HTTP basic auth, and an optional JSON body (POST).
#[allow(clippy::too_many_arguments)]
pub async fn send(
    client: &reqwest::Client,
    cfg: &Config,
    method: reqwest::Method,
    url: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    basic_auth: Option<(&str, &str)>,
    json_body: Option<&serde_json::Value>,
) -> anyhow::Result<reqwest::Response> {
    let retries = 2u32;
    let backoff = 1.5f64;
    let mut last_err: Option<anyhow::Error> = None;

    let caller_sets_ua = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"));

    for attempt in 0..=retries {
        let mut req = client
            .request(method.clone(), url)
            .timeout(Duration::from_secs(cfg.req_timeout));
        if !caller_sets_ua {
            req = req.header("User-Agent", UA);
        }
        if !query.is_empty() {
            req = req.query(query);
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some((u, p)) = basic_auth {
            req = req.basic_auth(u, Some(p));
        }
        if let Some(body) = json_body {
            req = req.json(body);
        }
        match req.send().await {
            Ok(resp) => {
                let code = resp.status().as_u16();
                if matches!(code, 429 | 500 | 502 | 503 | 504) && attempt < retries {
                    sleep_backoff(backoff, attempt).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                last_err = Some(e.into());
                if attempt < retries {
                    sleep_backoff(backoff, attempt).await;
                    continue;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("request failed")))
}

/// `_get` (badbitch2.py:209).
pub async fn get(
    client: &reqwest::Client,
    cfg: &Config,
    url: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
) -> anyhow::Result<reqwest::Response> {
    send(client, cfg, reqwest::Method::GET, url, query, headers, None, None).await
}

/// GET with HTTP basic auth (e.g. Censys id/secret).
pub async fn get_auth(
    client: &reqwest::Client,
    cfg: &Config,
    url: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    auth: (&str, &str),
) -> anyhow::Result<reqwest::Response> {
    send(client, cfg, reqwest::Method::GET, url, query, headers, Some(auth), None).await
}

/// POST a JSON body (e.g. IntelX, DeHashed v2).
pub async fn post_json(
    client: &reqwest::Client,
    cfg: &Config,
    url: &str,
    headers: &[(String, String)],
    body: &serde_json::Value,
    auth: Option<(&str, &str)>,
) -> anyhow::Result<reqwest::Response> {
    send(client, cfg, reqwest::Method::POST, url, &[], headers, auth, Some(body)).await
}

async fn sleep_backoff(backoff: f64, attempt: u32) {
    let secs = backoff.powi(attempt as i32 + 1);
    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
}

static RE_SCRIPT: LazyLock<Regex> = LazyLock::new(|| {
    // Rust's `regex` crate has no backreferences (unlike Python `re`), so spell out each
    // tag pair instead of `</\1>`. Equivalent to bs4 dropping script/style/noscript.
    Regex::new(
        r"(?is)<script\b[^>]*>.*?</script>|<style\b[^>]*>.*?</style>|<noscript\b[^>]*>.*?</noscript>",
    )
    .unwrap()
});
static RE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
static RE_WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\f\r]+").unwrap());

/// `_extract` (badbitch2.py:361): clean text from HTML. We replicate the bs4 fallback path
/// (strip script/style/noscript, then tags, then collapse to non-empty space-joined lines) —
/// trafilatura has no drop-in Rust equivalent.
pub fn extract(html: &str) -> String {
    let no_scripts = RE_SCRIPT.replace_all(html, " ");
    let no_tags = RE_TAG.replace_all(&no_scripts, " ");
    let decoded = decode_entities(&no_tags);
    decoded
        .lines()
        .map(|l| RE_WS.replace_all(l.trim(), " ").to_string())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// `_fetch_url_full` (badbitch2.py:377): plain GET + clean-extract, full text (no truncation).
/// No curl_cffi impersonation fallback in Rust — note the limitation and suggest fetch_rendered.
pub async fn fetch_url_full(client: &reqwest::Client, cfg: &Config, url: &str) -> String {
    let mut note = String::new();
    if cfg.tor {
        note.push_str("[via Tor] ");
    }
    let html = match get(client, cfg, url, &[], &[]).await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            if code == 200 && body.len() > 500 && !body.to_lowercase().contains("captcha") {
                Some(body)
            } else {
                note.push_str(&format!("plain GET status={code} len={}; ", body.len()));
                None
            }
        }
        Err(e) => {
            note.push_str(&format!("plain GET error: {e}; "));
            None
        }
    };
    let Some(html) = html else {
        return format!(
            "[fetch failed] {note}(no curl_cffi/impersonation in this build). If JS site, try fetch_rendered."
        );
    };
    let out = extract(&html);
    let head = out.chars().take(1500).collect::<String>().to_lowercase();
    if head.contains("captcha") || out.len() < 200 {
        note.push_str("Looks like a bot wall / JS page — consider fetch_rendered. ");
    }
    if note.is_empty() {
        out
    } else {
        format!("[{note}]\n{out}")
    }
}

/// `fetch_url` (badbitch2.py:404): truncated for inline use.
pub async fn fetch_url(client: &reqwest::Client, cfg: &Config, url: &str, max_chars: usize) -> String {
    let full = fetch_url_full(client, cfg, url).await;
    crate::util::truncate_chars(&full, max_chars.min(cfg.max_fetch_chars))
}

/// `fetch_json` (badbitch2.py:445): GET a JSON endpoint, return compact JSON (or raw text).
pub async fn fetch_json(
    client: &reqwest::Client,
    cfg: &Config,
    url: &str,
    params_json: &str,
    headers_json: &str,
) -> String {
    let query = match parse_str_map(params_json) {
        Ok(q) => q,
        Err(e) => return format!("[fetch_json error] bad params_json: {e}"),
    };
    let headers = match parse_str_map(headers_json) {
        Ok(h) => h,
        Err(e) => return format!("[fetch_json error] bad headers_json: {e}"),
    };
    match get(client, cfg, url, &query, &headers).await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => compact_json(&v, 5000),
                Err(_) => format!(
                    "[not JSON, status={code}]\n{}",
                    crate::util::truncate_chars(&text, 3000)
                ),
            }
        }
        Err(e) => format!("[fetch_json error] {e}"),
    }
}

fn parse_str_map(s: &str) -> anyhow::Result<Vec<(String, String)>> {
    if s.trim().is_empty() {
        return Ok(vec![]);
    }
    let v: Value = serde_json::from_str(s)?;
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected a JSON object"))?;
    Ok(obj
        .iter()
        .map(|(k, v)| {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), val)
        })
        .collect())
}

/// `_j` (badbitch2.py:213): compact-JSON a value, trimmed to a char budget.
pub fn compact_json(v: &Value, max_chars: usize) -> String {
    let s = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    clip_marked(&s, max_chars)
}

/// Read a response body and return compact JSON (or a `[not JSON …]` fallback). The common
/// `return _j(r.json(), max_chars=…)` tail of the API tools.
pub async fn resp_json_compact(resp: reqwest::Response, max_chars: usize) -> String {
    let code = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => compact_json(&v, max_chars),
        Err(_) => format!(
            "[not JSON, status={code}]\n{}",
            crate::util::truncate_chars(&text, 3000)
        ),
    }
}
