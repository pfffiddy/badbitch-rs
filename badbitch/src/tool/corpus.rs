//! Collect-to-disk corpus tools: collect (474), query_docs (495), read_doc (529),
//! plus `_doc_keywords` (465). Keeps the context window small on big/multi-source cases.

use std::sync::LazyLock;
use std::sync::atomic::Ordering;

use badbitch_macros::tool;
use chrono::Utc;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::http;
use crate::tool::ToolContext;
use crate::util::clip_result;

fn default_max_hits() -> u32 {
    40
}
fn default_length() -> u32 {
    2000
}

struct KwPat {
    label: &'static str,
    re: Regex,
}

static DOC_KW: LazyLock<Vec<KwPat>> = LazyLock::new(|| {
    let p = |label, pat| KwPat {
        label,
        re: Regex::new(pat).unwrap(),
    };
    vec![
        p("owner", r"(?i)owner"),
        p("R-acct", r"\bR0*\d{3,}\b"),
        p("parcel/APN", r"(?i)\b(apn|parcel|prop(?:erty)?\s*id|pin)\b"),
        p("value", r"(?i)assess|market value|appraised|taxable"),
        p("deed", r"(?i)\b(deed|grantor|grantee)\b"),
        p("tax", r"(?i)delinquen|tax\s+(due|year)"),
        p("email", r"[\w.+-]+@[\w.-]+\.\w+"),
        p("phone", r"\b\d{3}[-.\s]\d{3}[-.\s]\d{4}\b"),
    ]
});

/// `_doc_keywords` (badbitch2.py:465): flag OSINT-relevant fields present in a doc.
fn doc_keywords(text: &str) -> String {
    DOC_KW
        .iter()
        .filter(|k| k.re.is_match(text))
        .map(|k| k.label)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CollectInput {
    pub url: String,
    #[serde(default)]
    pub rendered: bool,
}

#[tool(
    name = "collect",
    description = "Fetch a page and store its FULL text to a local file, returning only a short receipt (doc id, size, key fields detected) — NOT the page body. Choose this for a large page, or one you may cite later, so it doesn't fill the context window. Then query_docs('pattern') greps across stored docs and read_doc(id) reads a slice. Set rendered=True for JS sites."
)]
pub async fn collect(ctx: ToolContext, input: CollectInput) -> String {
    let text = if input.rendered {
        crate::tool::web::fetch_rendered_full(&ctx, &input.url, "").await
    } else {
        http::fetch_url_full(&ctx.http, &ctx.config, &input.url).await
    };
    if let Err(e) = std::fs::create_dir_all(&ctx.docs_dir) {
        return format!("[collect error] {e}");
    }
    let seq = ctx.doc_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let path = ctx.docs_dir.join(format!("doc{seq}.txt"));
    let body = format!("URL: {}\nFETCHED: {}\n\n{text}", input.url, Utc::now().to_rfc3339());
    if let Err(e) = std::fs::write(&path, &body) {
        return format!("[collect error] {e}");
    }
    let kw = doc_keywords(&text);
    let kw = if kw.is_empty() { "none".to_string() } else { kw };
    format!(
        "doc{seq} saved <- {} ({} chars). key fields detected: {kw}. Next: query_docs('pattern') to search, or read_doc({seq}) to read a slice.",
        input.url,
        text.chars().count()
    )
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryDocsInput {
    pub pattern: String,
    #[serde(default = "default_max_hits")]
    pub max_hits: u32,
}

#[tool(
    name = "query_docs",
    description = "Search all docs saved by collect() for a regex/text pattern (case-insensitive); returns only matching lines with their doc id, so 'find the owner / R-number / assessed value' costs a few lines, not whole pages. Use after collect() to pull just the facts you need."
)]
pub async fn query_docs(ctx: ToolContext, input: QueryDocsInput) -> String {
    let dir = &ctx.docs_dir;
    let mut files: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".txt"))
            .collect(),
        Err(_) => return "[no collected docs yet — use collect(url) first]".to_string(),
    };
    if files.is_empty() {
        return "[no collected docs yet — use collect(url) first]".to_string();
    }
    files.sort();
    let max_hits = input.max_hits as usize;
    let rx = match Regex::new(&format!("(?i){}", input.pattern)) {
        Ok(r) => r,
        Err(e) => return format!("[query_docs: invalid regex: {e}]"),
    };
    let mut hits: Vec<String> = Vec::new();
    'outer: for fn_ in &files {
        let content = match std::fs::read_to_string(dir.join(fn_)) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (i, line) in content.lines().enumerate() {
            if rx.is_match(line) {
                let snippet: String = line.trim().chars().take(300).collect();
                hits.push(format!("{fn_}:{}:{snippet}", i + 1));
                if hits.len() > max_hits {
                    break 'outer;
                }
            }
        }
    }
    if hits.is_empty() {
        return format!("[no matches for /{}/ across {} doc(s)]", input.pattern, files.len());
    }
    let more = hits.len() > max_hits;
    let mut res = hits.into_iter().take(max_hits).collect::<Vec<_>>().join("\n");
    if more {
        res.push_str("\n…[more matches — refine the pattern]");
    }
    clip_result(&res, ctx.config.max_tool_result_chars)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadDocInput {
    pub doc_id: i64,
    #[serde(default)]
    pub start: u32,
    #[serde(default = "default_length")]
    pub length: u32,
}

#[tool(
    name = "read_doc",
    description = "Read a slice of one doc saved by collect() (by its number), returning `length` chars from `start`. Use to read the section a query_docs hit pointed to, without loading the whole page into context."
)]
pub async fn read_doc(ctx: ToolContext, input: ReadDocInput) -> String {
    let path = ctx.docs_dir.join(format!("doc{}.txt", input.doc_id));
    if !path.exists() {
        return format!("[no doc{} — collect() one first]", input.doc_id);
    }
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) => return format!("[read_doc error] {e}"),
    };
    let start = input.start as usize;
    let length = (input.length as usize).min(ctx.config.max_tool_result_chars);
    let sliced: String = data.chars().skip(start).take(length).collect();
    if sliced.is_empty() {
        "[empty / start beyond end of doc]".to_string()
    } else {
        sliced
    }
}
