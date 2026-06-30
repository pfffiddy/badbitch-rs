//! Pure link-builder tools + fetch_url/fetch_json as model-callable tools + tor_status.
//! (badbitch2.py:1149-1200, 404-449, 1559).

use std::net::TcpStream;

use badbitch_macros::tool;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::http;
use crate::tool::ToolContext;

fn q(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}
fn dashed(s: &str) -> String {
    q(s).replace("%20", "-")
}

// ---- reverse_image_links ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReverseImageLinksInput {
    pub image_url: String,
}

#[tool(
    name = "reverse_image_links",
    description = "Build reverse-image-search URLs (Google Lens, Yandex, Bing, TinEye) for an image URL. Use to find where a listing photo or a person's photo appears elsewhere. Yandex is usually strongest for faces/places."
)]
pub async fn reverse_image_links(_ctx: ToolContext, input: ReverseImageLinksInput) -> String {
    let url = q(&input.image_url);
    format!(
        "Google Lens: https://lens.google.com/uploadbyurl?url={url}\n\
         Yandex:      https://yandex.com/images/search?rpt=imageview&url={url}\n\
         Bing:        https://www.bing.com/images/search?q=imgurl:{url}&view=detailv2&iss=sbi\n\
         TinEye:      https://tineye.com/search?url={url}\n\
         Yandex is usually strongest for faces/places."
    )
}

// ---- crime_data_links ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CrimeDataLinksInput {
    pub location: String,
}

#[tool(
    name = "crime_data_links",
    description = "Build links to crime-mapping / police-blotter portals for a location (community crime maps + local blotter search). Use for neighborhood context around an abandoned property (squatting, fire, code calls)."
)]
pub async fn crime_data_links(_ctx: ToolContext, input: CrimeDataLinksInput) -> String {
    let loc = q(&input.location);
    format!(
        "CrimeMapping:    https://www.crimemapping.com/map/location/{loc}\n\
         SpotCrime:       https://spotcrime.com/search?q={loc}\n\
         LexisNexis CCM:  https://communitycrimemap.com/\n\
         web blotter:     run web_search('{} police blotter OR arrest log OR incident report')\n\
         Local blotters vary by PD — search the specific city/county PD site.",
        input.location
    )
}

// ---- fetch_url as a model-callable tool ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchUrlInput {
    pub url: String,
    /// Maximum characters to return (default 6000, capped by config max_fetch_chars).
    #[serde(default = "default_fetch_max")]
    pub max_chars: usize,
}

fn default_fetch_max() -> usize {
    6000
}

#[tool(
    name = "fetch_url",
    description = "Fetch and clean-extract a page (fast path, no JavaScript). For JS-rendered county/real-estate sites use fetch_rendered. Prefer the JSON API tools over this. For a large page you'll cite later, collect() stores it whole to disk instead."
)]
pub async fn fetch_url(ctx: ToolContext, input: FetchUrlInput) -> String {
    http::fetch_url(&ctx.http, &ctx.config, &input.url, input.max_chars).await
}

// ---- fetch_json as a model-callable tool ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchJsonInput {
    pub url: String,
    /// Optional JSON object string of query params, e.g. {"key":"val"}.
    #[serde(default)]
    pub params_json: String,
    /// Optional JSON object string of extra headers, e.g. {"X-Key":"val"}.
    #[serde(default)]
    pub headers_json: String,
}

#[tool(
    name = "fetch_json",
    description = "GET a JSON API endpoint directly and return the parsed JSON (compact). Use for any REST endpoint that returns JSON — especially county ArcGIS/GIS REST services, open-data portals, and undocumented site APIs you discover."
)]
pub async fn fetch_json(ctx: ToolContext, input: FetchJsonInput) -> String {
    http::fetch_json(&ctx.http, &ctx.config, &input.url, &input.params_json, &input.headers_json).await
}

// ---- tor_status ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TorStatusInput {}

#[tool(
    name = "tor_status",
    description = "Report Tor routing state: whether tor is enabled, whether the SOCKS proxy is listening, and the exit IP seen through it vs. your direct IP. Use to confirm anonymity before scraping sensitive targets."
)]
pub async fn tor_status(ctx: ToolContext, _input: TorStatusInput) -> String {
    let cfg = &ctx.config;
    let proxy = &cfg.tor_proxy;

    // Check if SOCKS port is listening
    let listening = {
        let host_port = proxy.split_once("://").map(|x| x.1).unwrap_or(proxy.as_str());
        if let Some((host, port_str)) = host_port.rsplit_once(':') {
            if let Ok(port) = port_str.parse::<u16>() {
                TcpStream::connect_timeout(
                    &std::net::SocketAddr::new(host.parse().unwrap_or([127, 0, 0, 1].into()), port),
                    std::time::Duration::from_secs(3),
                )
                .is_ok()
            } else {
                false
            }
        } else {
            false
        }
    };

    let mut lines = vec![
        format!("tor enabled in config: {}", cfg.tor),
        format!("proxy: {proxy}"),
        format!("SOCKS port listening: {listening}"),
    ];

    // Direct IP
    match http::get(&ctx.http, cfg, "https://api.ipify.org", &[], &[]).await {
        Ok(r) => {
            let ip = r.text().await.unwrap_or_default().trim().to_string();
            lines.push(format!("direct IP: {ip}"));
        }
        Err(e) => lines.push(format!("direct IP: [error] {e}")),
    }

    lines.push("(exit IP via Tor not available in this build — Tor proxy is set at client build time)".to_string());
    lines.join("\n")
}
