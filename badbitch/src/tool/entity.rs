//! People / entity intel (Phase 2): theharvester (942), phoneinfoga (1000), dehashed (1018),
//! rocketreach (1077), opencorporates (1094), breach_check (1117).

use badbitch_macros::tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::UA;
use crate::http;
use crate::shell;
use crate::tool::ToolContext;

fn default_harvester_sources() -> String {
    "duckduckgo,bing,crtsh,otx,hackertarget".to_string()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TheharvesterInput {
    pub domain: String,
    #[serde(default = "default_harvester_sources")]
    pub sources: String,
}

#[tool(
    name = "theharvester",
    description = "Run theHarvester against a domain to collect emails, subdomains, hosts, and names from public sources. Returns the parsed JSON results."
)]
pub async fn theharvester(ctx: ToolContext, input: TheharvesterInput) -> String {
    if !shell::have("theHarvester").await {
        return "[theHarvester missing] sudo apt install -y theharvester".to_string();
    }
    let to = ctx.config.long_tool_timeout;
    let tmp = std::env::temp_dir().join(format!("harv_{}", std::process::id()));
    let tmp = tmp.to_string_lossy().to_string();
    let result = shell::run(
        "theHarvester",
        &["-d", &input.domain, "-b", &input.sources, "-f", &tmp],
        to,
    )
    .await;
    let out = match result {
        Ok(o) if o.timed_out => format!("[theHarvester timeout after {to}s]"),
        Ok(_) => {
            let jf = format!("{tmp}.json");
            match std::fs::read_to_string(&jf).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                Some(data) => {
                    let mut slim = serde_json::Map::new();
                    for k in ["emails", "hosts", "ips", "people", "linkedin_people", "asns"] {
                        if let Some(v) = data.get(k)
                            && !v.is_null()
                        {
                            slim.insert(k.to_string(), v.clone());
                        }
                    }
                    let val = if slim.is_empty() { data } else { Value::Object(slim) };
                    http::compact_json(&val, 5000)
                }
                None => "[theHarvester ran but produced no JSON — try different -b sources]".to_string(),
            }
        }
        Err(e) => format!("[theHarvester error] {e}"),
    };
    for ext in [".json", ".xml"] {
        let _ = std::fs::remove_file(format!("{tmp}{ext}"));
    }
    out
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PhoneinfogaInput {
    pub number: String,
}

#[tool(
    name = "phoneinfoga",
    description = "Phone-number intelligence (phoneinfoga): carrier, line type, country/area, and footprint search links. Pass an E.164 number, e.g. '+14325550100'. Returns the scan."
)]
pub async fn phoneinfoga(ctx: ToolContext, input: PhoneinfogaInput) -> String {
    if !shell::have("phoneinfoga").await {
        return "[phoneinfoga missing] install:\n  curl -sSL https://raw.githubusercontent.com/sundowndev/phoneinfoga/master/support/install | bash\n  sudo mv ./phoneinfoga /usr/local/bin/".to_string();
    }
    let to = ctx.config.long_tool_timeout;
    match shell::run("phoneinfoga", &["scan", "-n", &input.number], to).await {
        Ok(o) if o.timed_out => format!("[phoneinfoga timeout after {to}s]"),
        Ok(o) => {
            let mut out = o.stdout;
            if !o.stderr.trim().is_empty() {
                out.push_str(&format!("\n[stderr]\n{}", o.stderr));
            }
            if out.trim().is_empty() {
                "[no output]".to_string()
            } else {
                crate::util::truncate_chars(&out, 6000)
            }
        }
        Err(e) => format!("[phoneinfoga error] {e}"),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DehashedInput {
    pub query_term: String,
}

#[tool(
    name = "dehashed",
    description = "DeHashed breach-data lookup — compromised credentials, leaked records, and linked assets for an email, username, name, phone, IP, domain, or password hash. Returns JSON. Needs dehashed_email + dehashed_key (paid). Use only for authorized investigation."
)]
pub async fn dehashed(ctx: ToolContext, input: DehashedInput) -> String {
    let email = ctx.config.key("dehashed_email");
    let k = ctx.config.key("dehashed_key");
    if email.is_empty() || k.is_empty() {
        return ctx.config.need_key("dehashed_email / dehashed_key", "DeHashed", "https://dehashed.com/ (account -> API)");
    }
    let headers = vec![
        ("Dehashed-Api-Key".into(), k.clone()),
        ("Accept".into(), "application/json".into()),
        ("Content-Type".into(), "application/json".into()),
    ];
    let body = json!({"query": input.query_term, "size": 20});
    let v2 = http::post_json(&ctx.http, &ctx.config, "https://api.dehashed.com/v2/search", &headers, &body, None).await;
    match v2 {
        Ok(r) if r.status().as_u16() == 200 => http::resp_json_compact(r, 5000).await,
        Ok(r) => {
            let s2 = r.status().as_u16();
            // v2 rejected -> legacy v1 (GET + basic auth)
            match http::get_auth(
                &ctx.http,
                &ctx.config,
                "https://api.dehashed.com/search",
                &[("query".into(), input.query_term.clone())],
                &[("Accept".into(), "application/json".into())],
                (&email, &k),
            )
            .await
            {
                Ok(r2) if r2.status().as_u16() == 200 => http::resp_json_compact(r2, 5000).await,
                Ok(r2) => format!("[dehashed] v2 status={s2}, v1 status={}", r2.status().as_u16()),
                Err(e) => format!("[dehashed error] {e}"),
            }
        }
        Err(e) => format!("[dehashed error] {e}"),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RocketreachInput {
    pub name: String,
    #[serde(default)]
    pub company: String,
}

#[tool(
    name = "rocketreach",
    description = "RocketReach person lookup — professional email/phone/title for a named person (optionally at a company). Returns JSON contact. Needs rocketreach key (paid/trial)."
)]
pub async fn rocketreach(ctx: ToolContext, input: RocketreachInput) -> String {
    let k = ctx.config.key("rocketreach");
    if k.is_empty() {
        return ctx.config.need_key("rocketreach", "RocketReach", "https://rocketreach.co/api");
    }
    let mut query = vec![("name".into(), input.name.clone())];
    if !input.company.is_empty() {
        query.push(("current_employer".into(), input.company.clone()));
    }
    match http::get(&ctx.http, &ctx.config, "https://api.rocketreach.co/api/v2/person/lookup", &query, &[("Api-Key".into(), k)]).await {
        Ok(r) => http::resp_json_compact(r, 4500).await,
        Err(e) => format!("[rocketreach error] {e}"),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OpencorporatesInput {
    pub query: String,
    #[serde(default)]
    pub jurisdiction: String,
}

#[tool(
    name = "opencorporates",
    description = "Search OpenCorporates for companies (officers, registered agent, status, filings). Use when a property/domain owner is an LLC. jurisdiction optional (e.g. 'us_tx'). Returns JSON. Works without a key at a low rate; add opencorporates for higher limits."
)]
pub async fn opencorporates(ctx: ToolContext, input: OpencorporatesInput) -> String {
    let mut query = vec![("q".into(), input.query.clone()), ("per_page".into(), "8".into())];
    let k = ctx.config.key("opencorporates");
    if !k.is_empty() {
        query.push(("api_token".into(), k));
    }
    if !input.jurisdiction.is_empty() {
        query.push(("jurisdiction_code".into(), input.jurisdiction.clone()));
    }
    let resp = match http::get(&ctx.http, &ctx.config, "https://api.opencorporates.com/v0.4/companies/search", &query, &[]).await {
        Ok(r) => r,
        Err(e) => return format!("[opencorporates error] {e}"),
    };
    let data: Value = match serde_json::from_str(&resp.text().await.unwrap_or_default()) {
        Ok(v) => v,
        Err(e) => return format!("[opencorporates error] {e}"),
    };
    let companies = data
        .get("results")
        .and_then(|r| r.get("companies"))
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let slim: Vec<Value> = companies
        .iter()
        .filter_map(|c| c.get("company"))
        .map(|c| {
            json!({
                "name": c.get("name"),
                "company_number": c.get("company_number"),
                "jurisdiction_code": c.get("jurisdiction_code"),
                "current_status": c.get("current_status"),
                "incorporation_date": c.get("incorporation_date"),
                "registered_address_in_full": c.get("registered_address_in_full"),
                "opencorporates_url": c.get("opencorporates_url"),
            })
        })
        .collect();
    if slim.is_empty() {
        http::compact_json(&data, 2500)
    } else {
        format!("[{} company(ies)]\n{}", slim.len(), http::compact_json(&Value::Array(slim), 4500))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BreachCheckInput {
    pub email: String,
}

#[tool(
    name = "breach_check",
    description = "Check Have I Been Pwned for breaches exposing an email. Returns JSON breach list. Needs hibp key (paid)."
)]
pub async fn breach_check(ctx: ToolContext, input: BreachCheckInput) -> String {
    let k = ctx.config.key("hibp");
    if k.is_empty() {
        return ctx.config.need_key("hibp", "HaveIBeenPwned", "https://haveibeenpwned.com/API/Key");
    }
    let url = format!("https://haveibeenpwned.com/api/v3/breachedaccount/{}", input.email);
    let headers = vec![("hibp-api-key".into(), k), ("User-Agent".into(), UA.to_string())];
    match http::get(&ctx.http, &ctx.config, &url, &[("truncateResponse".into(), "false".into())], &headers).await {
        Ok(r) if r.status().as_u16() == 404 => "[no breaches found]".to_string(),
        Ok(r) => http::resp_json_compact(r, 4000).await,
        Err(e) => format!("[hibp error] {e}"),
    }
}
