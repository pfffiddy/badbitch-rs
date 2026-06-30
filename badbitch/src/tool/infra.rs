//! Infrastructure / domain intel (Phase 2): shodan (784), censys (803), dnsdumpster (820),
//! virustotal (833), intelx (858), dns_recon (907).

use std::sync::LazyLock;
use std::time::Duration;

use badbitch_macros::tool;
use base64::Engine;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::classify::{is_ip, looks_domain};
use crate::config::UA;
use crate::http;
use crate::shell;
use crate::tool::ToolContext;

static RE_HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Fa-f0-9]{32,64}$").unwrap());

fn default_intelx_max() -> u32 {
    10
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShodanInput {
    pub target: String,
}

#[tool(
    name = "shodan",
    description = "Shodan lookup. If target is an IP -> host detail (open ports, services, banners, CVEs). If a domain -> DNS/subdomain data. Otherwise -> host search query. Returns JSON."
)]
pub async fn shodan(ctx: ToolContext, input: ShodanInput) -> String {
    let k = ctx.config.key("shodan");
    if k.is_empty() {
        return ctx.config.need_key("shodan", "Shodan", "https://account.shodan.io/");
    }
    let t = input.target.trim();
    let (url, query) = if is_ip(t) {
        (format!("https://api.shodan.io/shodan/host/{t}"), vec![("key".into(), k.clone())])
    } else if looks_domain(t) {
        (format!("https://api.shodan.io/dns/domain/{t}"), vec![("key".into(), k.clone())])
    } else {
        (
            "https://api.shodan.io/shodan/host/search".to_string(),
            vec![("key".into(), k.clone()), ("query".into(), t.to_string())],
        )
    };
    match http::get(&ctx.http, &ctx.config, &url, &query, &[]).await {
        Ok(r) => http::resp_json_compact(r, 5000).await,
        Err(e) => format!("[shodan error] {e}"),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CensysInput {
    pub target: String,
}

#[tool(
    name = "censys",
    description = "Censys lookup (Search API v2). IP -> host detail; otherwise -> host search query. Surfaces services, certs, ASN, geo. Returns JSON. Needs censys_id + censys_secret."
)]
pub async fn censys(ctx: ToolContext, input: CensysInput) -> String {
    let cid = ctx.config.key("censys_id");
    let csec = ctx.config.key("censys_secret");
    if cid.is_empty() || csec.is_empty() {
        return ctx.config.need_key("censys_id / censys_secret", "Censys", "https://search.censys.io/account/api");
    }
    let t = input.target.trim();
    let res = if is_ip(t) {
        http::get_auth(&ctx.http, &ctx.config, &format!("https://search.censys.io/api/v2/hosts/{t}"), &[], &[], (&cid, &csec)).await
    } else {
        http::get_auth(
            &ctx.http,
            &ctx.config,
            "https://search.censys.io/api/v2/hosts/search",
            &[("q".into(), t.to_string()), ("per_page".into(), "10".into())],
            &[],
            (&cid, &csec),
        )
        .await
    };
    match res {
        Ok(r) => http::resp_json_compact(r, 5000).await,
        Err(e) => format!("[censys error] {e} (note: Censys is migrating to platform.censys.io; update endpoint if v2 is retired)"),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DnsdumpsterInput {
    pub domain: String,
}

#[tool(
    name = "dnsdumpster",
    description = "DNSDumpster passive DNS / subdomain map for a domain (A, NS, MX, TXT, CNAME). Returns JSON. Needs a dnsdumpster API key."
)]
pub async fn dnsdumpster(ctx: ToolContext, input: DnsdumpsterInput) -> String {
    let k = ctx.config.key("dnsdumpster");
    if k.is_empty() {
        return ctx.config.need_key("dnsdumpster", "DNSDumpster", "https://dnsdumpster.com/ (account -> API)");
    }
    let url = format!("https://api.dnsdumpster.com/domain/{}", input.domain);
    match http::get(&ctx.http, &ctx.config, &url, &[], &[("X-API-Key".into(), k)]).await {
        Ok(r) => http::resp_json_compact(r, 5000).await,
        Err(e) => format!("[dnsdumpster error] {e}"),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VirustotalInput {
    pub indicator: String,
}

#[tool(
    name = "virustotal",
    description = "VirusTotal v3 lookup. Auto-detects IP / domain / file-hash / URL. Returns reputation, resolutions, detections, WHOIS, related samples. Returns JSON. Needs virustotal key."
)]
pub async fn virustotal(ctx: ToolContext, input: VirustotalInput) -> String {
    let k = ctx.config.key("virustotal");
    if k.is_empty() {
        return ctx.config.need_key("virustotal", "VirusTotal", "https://www.virustotal.com/gui/my-apikey");
    }
    let ind = input.indicator.trim();
    let url = if is_ip(ind) {
        format!("https://www.virustotal.com/api/v3/ip_addresses/{ind}")
    } else if RE_HASH.is_match(ind) {
        format!("https://www.virustotal.com/api/v3/files/{ind}")
    } else if ind.to_lowercase().starts_with("http") {
        let uid = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(ind.as_bytes());
        format!("https://www.virustotal.com/api/v3/urls/{uid}")
    } else {
        format!("https://www.virustotal.com/api/v3/domains/{ind}")
    };
    match http::get(&ctx.http, &ctx.config, &url, &[], &[("x-apikey".into(), k)]).await {
        Ok(r) => http::resp_json_compact(r, 5000).await,
        Err(e) => format!("[virustotal error] {e}"),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IntelxInput {
    pub term: String,
    #[serde(default = "default_intelx_max")]
    pub max_results: u32,
}

#[tool(
    name = "intelx",
    description = "Intelligence X search across leaked data, pastes, darkweb, WHOIS history for a selector (email, domain, IP, phone, bitcoin addr, etc.). Two-step search; returns matching records as JSON. Needs intelx key."
)]
pub async fn intelx(ctx: ToolContext, input: IntelxInput) -> String {
    let k = ctx.config.key("intelx");
    if k.is_empty() {
        return ctx.config.need_key("intelx", "Intelligence X", "https://intelx.io/account?tab=developer");
    }
    let base = {
        let b = ctx.config.key("intelx_base");
        if b.is_empty() { "https://2.intelx.io".to_string() } else { b }
    };
    let base = base.trim_end_matches('/').to_string();
    let headers = vec![("x-key".into(), k), ("User-Agent".into(), UA.to_string())];
    let max = input.max_results;
    let body = json!({"term": input.term, "maxresults": max, "media": 0, "sort": 2, "terminate": []});

    let sid = match http::post_json(&ctx.http, &ctx.config, &format!("{base}/intelligent/search"), &headers, &body, None).await {
        Ok(r) => {
            let text = r.text().await.unwrap_or_default();
            match serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("id").and_then(|i| i.as_str().map(str::to_string))) {
                Some(id) => id,
                None => return format!("[intelx] no search id returned: {}", crate::util::truncate_chars(&text, 300)),
            }
        }
        Err(e) => return format!("[intelx error] {e}"),
    };

    let mut records: Vec<Value> = vec![];
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let res = http::get(
            &ctx.http,
            &ctx.config,
            &format!("{base}/intelligent/search/result"),
            &[("id".into(), sid.clone()), ("limit".into(), max.to_string())],
            &headers,
        )
        .await;
        if let Ok(r) = res
            && let Ok(jr) = serde_json::from_str::<Value>(&r.text().await.unwrap_or_default())
        {
            if let Some(recs) = jr.get("records").and_then(|v| v.as_array())
                && !recs.is_empty()
            {
                records = recs.clone();
            }
            let status = jr.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
            if (status == 1 || status == 2) && !records.is_empty() {
                break;
            }
        }
    }
    let slim: Vec<Value> = records
        .iter()
        .take(max as usize)
        .map(|x| {
            json!({
                "name": x.get("name"),
                "date": x.get("date"),
                "bucket": x.get("bucket"),
                "media": x.get("media"),
                "systemid": x.get("systemid"),
            })
        })
        .collect();
    format!("[{} record(s)]\n{}", records.len(), http::compact_json(&Value::Array(slim), 4500))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DnsReconInput {
    pub domain: String,
}

#[tool(
    name = "dns_recon",
    description = "Resolve DNS records (A/AAAA/MX/NS/TXT) + WHOIS registration for a domain using local dig/whois. Fast structured infrastructure footprint. Returns text."
)]
pub async fn dns_recon(_ctx: ToolContext, input: DnsReconInput) -> String {
    let domain = input.domain.trim();
    let mut out: Vec<String> = Vec::new();
    if shell::have("dig").await {
        for rt in ["A", "AAAA", "MX", "NS", "TXT"] {
            if let Ok(o) = shell::run("dig", &["+short", domain, rt], 20).await {
                let vals: Vec<&str> = o.stdout.lines().filter(|l| !l.trim().is_empty()).take(8).collect();
                if !vals.is_empty() {
                    out.push(format!("{rt}: {}", vals.join(", ")));
                }
            }
        }
    } else {
        match tokio::net::lookup_host(format!("{domain}:0")).await {
            Ok(addrs) => {
                let ips: std::collections::BTreeSet<String> =
                    addrs.map(|a| a.ip().to_string()).collect();
                out.push(format!("A: {}", ips.into_iter().collect::<Vec<_>>().join(", ")));
            }
            Err(e) => out.push(format!("[resolve error] {e}")),
        }
    }
    if shell::have("whois").await
        && let Ok(o) = shell::run("whois", &[domain], 30).await
    {
        let keys = [
            "Registrar:", "Creation Date:", "Updated Date:", "Registry Expiry",
            "Registrant", "Name Server:", "Domain Status:",
        ];
        let mut seen = std::collections::BTreeSet::new();
        let wl: Vec<String> = o
            .stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| keys.iter().any(|k| l.starts_with(k)))
            .filter(|l| seen.insert(l.clone()))
            .collect();
        if wl.is_empty() {
            out.push("[whois: no headline fields]".to_string());
        } else {
            out.push(format!("\n[whois]\n{}", crate::util::truncate_chars(&wl.join("\n"), 2000)));
        }
    }
    if out.is_empty() {
        "[no DNS/WHOIS data]".to_string()
    } else {
        out.join("\n")
    }
}
