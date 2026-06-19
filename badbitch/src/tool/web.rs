//! Web tools: web_search (329), recon_sweep (1306), fetch_rendered (438), wayback (890).
//!
//! recon_sweep reuses the other handlers directly — the `#[tool]` macro leaves each annotated
//! `async fn` callable as a plain function (it only *adds* a wrapper struct).

use std::collections::BTreeSet;
use std::sync::LazyLock;

use badbitch_macros::tool;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::classify;
use crate::config::UA;
use crate::http;
use crate::shell;
use crate::tool::ToolContext;
use crate::tool::corpus::{CollectInput, collect};
use crate::tool::people::{PeopleSearchLinksInput, people_search_links};
use crate::util::truncate_chars;

fn default_max_results() -> u32 {
    8
}
fn default_wayback_limit() -> u32 {
    15
}
fn default_recon_max_docs() -> u32 {
    6
}

static RE_URL_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"URL:\s*(\S+)").unwrap());

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebSearchInput {
    pub query: String,
    #[serde(default = "default_max_results")]
    pub max_results: u32,
}

#[tool(
    name = "web_search",
    description = "Search the web via the local SearXNG instance. Use to find county appraisal records, tax/foreclosure lists, obituaries, business filings, news, social profiles, etc. Returns title / url / snippet lines."
)]
pub async fn web_search(ctx: ToolContext, input: WebSearchInput) -> String {
    let cfg = &ctx.config;
    let max = input.max_results as usize;
    let query = vec![
        ("q".to_string(), input.query.clone()),
        ("format".to_string(), "json".to_string()),
    ];
    let searx_err: String = match http::get(&ctx.http, cfg, &cfg.searx_url, &query, &[]).await {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<Value>().await {
                    Ok(v) => {
                        let hits = v
                            .get("results")
                            .and_then(|r| r.as_array())
                            .cloned()
                            .unwrap_or_default();
                        if !hits.is_empty() {
                            return hits
                                .iter()
                                .take(max)
                                .map(|h| {
                                    let g = |k: &str| h.get(k).and_then(|v| v.as_str()).unwrap_or("");
                                    let snippet: String = g("content").chars().take(300).collect();
                                    format!(
                                        "URL: {}\nTITLE: {}\nSNIPPET: {}\n---",
                                        g("url"),
                                        g("title"),
                                        snippet
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                        }
                        "no results".to_string()
                    }
                    Err(e) => e.to_string(),
                }
            } else {
                format!("HTTP {}", resp.status().as_u16())
            }
        }
        Err(e) => e.to_string(),
    };
    format!(
        "[search failed] searxng: {searx_err}; no DDGS fallback in this build. Is SearXNG up on {}?",
        cfg.searx_url
    )
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaybackInput {
    pub url: String,
    #[serde(default = "default_wayback_limit")]
    pub limit: u32,
}

#[tool(
    name = "wayback",
    description = "Wayback Machine history for a URL — snapshot timestamps to recover deleted listings, obituaries, business pages, or see prior versions of a page. Returns JSON snapshots."
)]
pub async fn wayback(ctx: ToolContext, input: WaybackInput) -> String {
    let query = vec![
        ("url".to_string(), input.url.clone()),
        ("output".to_string(), "json".to_string()),
        ("limit".to_string(), input.limit.to_string()),
        ("collapse".to_string(), "digest".to_string()),
        ("fl".to_string(), "timestamp,original,statuscode".to_string()),
    ];
    let resp = match http::get(
        &ctx.http,
        &ctx.config,
        "http://web.archive.org/cdx/search/cdx",
        &query,
        &[],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return format!("[wayback error] {e}"),
    };
    let rows: Vec<Vec<String>> = match resp.json::<Value>().await {
        Ok(v) => v
            .as_array()
            .map(|outer| {
                outer
                    .iter()
                    .map(|row| {
                        row.as_array()
                            .map(|cells| {
                                cells
                                    .iter()
                                    .map(|c| c.as_str().unwrap_or("").to_string())
                                    .collect()
                            })
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Err(e) => return format!("[wayback error] {e}"),
    };
    if rows.len() <= 1 {
        return format!("[no Wayback snapshots] {}", input.url);
    }
    rows[1..]
        .iter()
        .filter(|r| r.len() >= 3)
        .map(|r| {
            format!(
                "{}  http://web.archive.org/web/{}/{}  [{}]",
                r[0], r[0], r[1], r[2]
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchRenderedInput {
    pub url: String,
    #[serde(default = "default_max_chars")]
    pub max_chars: u32,
    #[serde(default)]
    pub wait_selector: String,
}
fn default_max_chars() -> u32 {
    6000
}

const PLAYWRIGHT_HELPER: &str = r#"
import sys
try:
    from playwright.sync_api import sync_playwright
except Exception:
    sys.stderr.write("NO_PLAYWRIGHT"); sys.exit(3)
url = sys.argv[1]
sel = sys.argv[2] if len(sys.argv) > 2 else ""
proxy = sys.argv[3] if len(sys.argv) > 3 else ""
with sync_playwright() as p:
    kw = {"headless": True}
    if proxy:
        kw["proxy"] = {"server": proxy}
    b = p.chromium.launch(**kw)
    pg = b.new_page(user_agent="UA_PLACEHOLDER")
    pg.goto(url, wait_until="networkidle", timeout=30000)
    if sel:
        try: pg.wait_for_selector(sel, timeout=8000)
        except Exception: pass
    sys.stdout.write(pg.content())
    b.close()
"#;

/// `_fetch_rendered_full` (badbitch2.py:412): Playwright fetch + clean-extract, full text
/// (no truncation). Error/unavailable cases come back as a `[fetch_rendered …]` string.
pub async fn fetch_rendered_full(ctx: &ToolContext, url: &str, wait_selector: &str) -> String {
    let cfg = &ctx.config;
    if !shell::have("python3").await {
        return "[fetch_rendered unavailable] python3 + Playwright required:\n  pip install --user playwright && python3 -m playwright install chromium".to_string();
    }
    let script = PLAYWRIGHT_HELPER.replace("UA_PLACEHOLDER", UA);
    let proxy = if cfg.tor {
        cfg.tor_proxy.replace("socks5h://", "socks5://")
    } else {
        String::new()
    };
    let args = vec!["-c", script.as_str(), url, wait_selector, proxy.as_str()];
    match shell::run("python3", &args, 60).await {
        Ok(o) if o.timed_out => "[fetch_rendered error] timeout after 60s".to_string(),
        Ok(o) => {
            if o.stderr.contains("NO_PLAYWRIGHT") {
                return "[fetch_rendered unavailable] Playwright not installed:\n  pip install --user playwright && python3 -m playwright install chromium".to_string();
            }
            if o.stdout.trim().is_empty() {
                return format!("[fetch_rendered error] {}", o.stderr.trim());
            }
            http::extract(&o.stdout)
        }
        Err(e) => format!("[fetch_rendered error] {e}"),
    }
}

#[tool(
    name = "fetch_rendered",
    description = "Fetch a JavaScript-rendered page with headless Chromium (Playwright), then clean-extract. Use for county appraisal sites, Zillow, Redfin, and anything where fetch_url returns empty/blocked. Optionally wait for a CSS selector."
)]
pub async fn fetch_rendered(ctx: ToolContext, input: FetchRenderedInput) -> String {
    let cfg = &ctx.config;
    let full = fetch_rendered_full(&ctx, &input.url, &input.wait_selector).await;
    if full.starts_with("[fetch_rendered") {
        return full;
    }
    truncate_chars(&full, (input.max_chars as usize).min(cfg.max_fetch_chars))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReconSweepInput {
    pub target: String,
    #[serde(default)]
    pub location: String,
    #[serde(default = "default_recon_max_docs")]
    pub max_docs: u32,
}

#[tool(
    name = "recon_sweep",
    description = "PRE-FLIGHT aggregation: classify a raw target, fan out a batch of web searches and archive the top pages to disk (collect), returning a compact digest — NOT page bodies. The deterministic 'gather first' pass so you start from a corpus to query_docs()/read_doc() instead of fetching reactively. Subject-anchored: a named person stays the subject. Call this first on any new target."
)]
pub async fn recon_sweep(ctx: ToolContext, input: ReconSweepInput) -> String {
    let info = classify::classify_target(&input.target);
    let kind = info.kind.clone();
    let name = info.name.clone();
    let addr = info.address.clone();
    let dob = info.dob.clone();
    let loc = input.location.trim().to_string();
    let max_docs = input.max_docs as usize;

    let queries: Vec<String> = match kind.as_str() {
        "person" => vec![
            format!("{} {}", name, if loc.is_empty() { &addr } else { &loc }).trim().to_string(),
            format!("\"{name}\" {dob}").trim().to_string(),
            format!("{name} obituary OR probate OR relatives"),
            if loc.is_empty() {
                format!("{name} address OR phone")
            } else {
                format!("{name} {loc}")
            },
        ],
        "address" => vec![format!("{addr} owner"), format!("{addr} property records {loc}").trim().to_string()],
        "email" => vec![info.email.clone(), format!("\"{}\" profile OR breach OR leak", info.email)],
        "domain" => vec![info.domain.clone(), format!("{} whois OR subdomains OR security", info.domain)],
        "ip" => vec![info.ip.clone(), format!("{} abuse OR hosting OR shodan", info.ip)],
        "username" => vec![info.username.clone(), format!("\"{}\" profile OR account", info.username)],
        _ => vec![input.target.clone()],
    };

    let mut lines = vec![format!("# recon_sweep — kind={kind}")];
    if !name.is_empty() {
        let mut l = format!("subject (PERSON, stay anchored here): {name}");
        if !dob.is_empty() {
            l.push_str(&format!(" | DOB {dob}"));
        }
        if !addr.is_empty() {
            l.push_str(&format!(" | addr {addr}"));
        }
        lines.push(l);
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut collected = 0usize;
    for qstr in queries.into_iter().filter(|q| !q.is_empty()) {
        if collected >= max_docs {
            break;
        }
        let res = web_search(
            ctx.clone(),
            WebSearchInput { query: qstr.clone(), max_results: 4 },
        )
        .await;
        let urls: Vec<String> = RE_URL_LINE
            .captures_iter(&res)
            .map(|c| c[1].to_string())
            .filter(|u| !seen.contains(u))
            .collect();
        lines.push(format!("\n## searched: {qstr}  ({} new hit(s))", urls.len()));
        for url in urls {
            if collected >= max_docs {
                break;
            }
            seen.insert(url.clone());
            let receipt = collect(
                ctx.clone(),
                CollectInput { url: url.clone(), rendered: false },
            )
            .await;
            lines.push(format!("  - {receipt}"));
            collected += 1;
        }
    }

    if kind == "person" && !name.is_empty() {
        lines.push("\n## person leads (open in browser / fetch_rendered — corroborate, not ground truth)".to_string());
        lines.push(
            people_search_links(
                ctx.clone(),
                PeopleSearchLinksInput { name: name.clone(), location: loc.clone() },
            )
            .await,
        );
    }
    let anchor = if !name.is_empty() { name.clone() } else { input.target.clone() };
    lines.push(format!(
        "\n[{collected} doc(s) archived. Next: query_docs('pattern'), read_doc(id). Stay anchored on: {anchor}.]"
    ));

    let out = lines.join("\n");
    if out.chars().count() <= 3500 {
        out
    } else {
        format!("{}\n…[digest truncated]", truncate_chars(&out, 3500))
    }
}
