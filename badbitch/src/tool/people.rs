//! People / entity tools: people_search_links (1149), social_search_links (1175),
//! sherlock (969), holehe (985), extract_contacts (1134).

use std::collections::BTreeSet;
use std::sync::LazyLock;

use badbitch_macros::tool;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::http;
use crate::shell;
use crate::tool::ToolContext;

fn q(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}
fn dashed(s: &str) -> String {
    q(s).replace("%20", "-")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PeopleSearchLinksInput {
    pub name: String,
    #[serde(default)]
    pub location: String,
}

#[tool(
    name = "people_search_links",
    description = "Build query URLs for people-search aggregators (WhitePages, BeenVerified, TruePeopleSearch, FastPeopleSearch, etc.). These have NO free API and aggressively block scraping — open them in a browser or pass to fetch_rendered. Used to corroborate an owner/heir name from deeds."
)]
pub async fn people_search_links(_ctx: ToolContext, input: PeopleSearchLinksInput) -> String {
    let name = &input.name;
    let loc = q(&input.location);
    format!(
        "TruePeopleSearch: https://www.truepeoplesearch.com/results?name={}&citystatezip={}\n\
         FastPeopleSearch: https://www.fastpeoplesearch.com/name/{}\n\
         WhitePages:       https://www.whitepages.com/name/{}\n\
         BeenVerified:     https://www.beenverified.com/people/{}/\n\
         That'sThem:       https://thatsthem.com/name/{}\n\
         Note: ToS-restricted, often CAPTCHA-walled. Use as leads to corroborate via PRIMARY \
         records (county deeds, obituaries, probate), not as sources of truth.",
        q(name),
        loc,
        dashed(name),
        dashed(name),
        dashed(name),
        dashed(name),
    )
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SocialSearchLinksInput {
    pub query: String,
}

#[tool(
    name = "social_search_links",
    description = "Build search URLs for social platforms (X/Twitter, Instagram, Facebook, LinkedIn, Reddit) for a name/handle/keyword. These are the free web entry points (the X API is paid). For deep cross-platform handle hunting use sherlock."
)]
pub async fn social_search_links(_ctx: ToolContext, input: SocialSearchLinksInput) -> String {
    let s = q(&input.query);
    format!(
        "X/Twitter search: https://twitter.com/search?q={s}&f=live\n\
         X advanced:       https://twitter.com/search-advanced\n\
         Instagram:        https://www.instagram.com/explore/search/keyword/?q={s}\n\
         Facebook:         https://www.facebook.com/search/top?q={s}\n\
         LinkedIn:         https://www.linkedin.com/search/results/all/?keywords={s}\n\
         Reddit:           https://www.reddit.com/search/?q={s}\n\
         Note: cross-platform facial-recognition tooling is a separate CLI; run via run_shell only if installed and authorized."
    )
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SherlockInput {
    pub username: String,
}

#[tool(
    name = "sherlock",
    description = "Hunt a username across ~400 social/media sites (sherlock). Returns the list of sites where the account exists. Use to pivot from a handle to a person's footprint."
)]
pub async fn sherlock(ctx: ToolContext, input: SherlockInput) -> String {
    if !shell::have("sherlock").await {
        return "[sherlock missing] pipx install sherlock-project  (or pip install --user sherlock-project)".to_string();
    }
    let to = ctx.config.long_tool_timeout;
    match shell::run(
        "sherlock",
        &["--print-found", "--no-color", "--timeout", "8", &input.username],
        to,
    )
    .await
    {
        Ok(o) if o.timed_out => format!("[sherlock timeout after {to}s]"),
        Ok(o) => {
            let found: Vec<&str> = o
                .stdout
                .lines()
                .filter(|l| l.trim_start().starts_with("[+]"))
                .collect();
            let body = if found.is_empty() {
                "[no accounts found]".to_string()
            } else {
                found.join("\n")
            };
            if o.stderr.trim().is_empty() {
                body
            } else {
                format!("{body}\n[note] {}", &o.stderr.chars().take(200).collect::<String>())
            }
        }
        Err(e) => format!("[sherlock error] {e}"),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HoleheInput {
    pub email: String,
}

#[tool(
    name = "holehe",
    description = "Check which sites/services an email is registered on (holehe — the free Epieos-style email-to-account check). Returns the list of services where the email is used."
)]
pub async fn holehe(ctx: ToolContext, input: HoleheInput) -> String {
    if !shell::have("holehe").await {
        return "[holehe missing] pipx install holehe  (or pip install --user holehe)".to_string();
    }
    let to = ctx.config.long_tool_timeout;
    match shell::run("holehe", &["--only-used", &input.email], to).await {
        Ok(o) if o.timed_out => format!("[holehe timeout after {to}s]"),
        Ok(o) => {
            let n = o.stdout.chars().count();
            if n == 0 {
                "[no used accounts found]".to_string()
            } else {
                o.stdout.chars().skip(n.saturating_sub(5000)).collect()
            }
        }
        Err(e) => format!("[holehe error] {e}"),
    }
}

static RE_EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap());
static RE_PHONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}").unwrap()
});
static RE_HANDLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@[A-Za-z0-9_]{2,30}\b").unwrap());

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtractContactsInput {
    pub text_or_url: String,
}

#[tool(
    name = "extract_contacts",
    description = "Pull emails, US phone numbers, and social handles from raw text OR a URL (fetched first). Useful for grabbing a listing agent, county office, or registered-agent contact."
)]
pub async fn extract_contacts(ctx: ToolContext, input: ExtractContactsInput) -> String {
    let txt = if input.text_or_url.to_lowercase().starts_with("http") {
        http::fetch_url(&ctx.http, &ctx.config, &input.text_or_url, 20000).await
    } else {
        input.text_or_url.clone()
    };
    let collect = |re: &Regex| -> BTreeSet<String> {
        re.find_iter(&txt).map(|m| m.as_str().to_string()).collect()
    };
    let emails: Vec<String> = collect(&RE_EMAIL).into_iter().collect();
    let phones: Vec<String> = collect(&RE_PHONE).into_iter().collect();
    let handles: Vec<String> = collect(&RE_HANDLE).into_iter().collect();
    let fmt = |v: &[String]| if v.is_empty() { "none".to_string() } else { format!("{v:?}") };
    format!(
        "emails: {}\nphones: {}\nhandles: {}",
        fmt(&emails),
        fmt(&phones),
        fmt(&handles)
    )
}
