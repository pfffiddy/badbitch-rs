//! Property tools: find_county_portals (754) + KNOWN_PORTALS (733), arcgis_query (706).
//! (attom_property / regrid_parcel are Phase 2.)

use std::sync::LazyLock;

use badbitch_macros::tool;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::http;
use crate::tool::ToolContext;
use crate::tool::web::{WebSearchInput, web_search};

struct Portal {
    appraisal_district: &'static str,
    parcel_arcgis: &'static str,
    parcel_fields: &'static str,
    parcel_lookup: &'static str,
    property_search: &'static str,
    gis_map: &'static str,
}

static KNOWN_PORTALS: LazyLock<Vec<(&'static str, Portal)>> = LazyLock::new(|| {
    vec![(
        "midland, tx",
        Portal {
            appraisal_district: "https://midcad.org/",
            parcel_arcgis: "https://maps.midlandtexas.gov/arcgis/rest/services/ParcelLink/MapServer/0",
            parcel_fields: "PIN, LONG_R (R-account, e.g. R000043833), SHORT_R, GEOID, StreetNum, StreetName, StreetType, City, Acres, Jurisdiction",
            parcel_lookup: "filter on StreetNum + StreetName (UPPERCASE), e.g. where=\"StreetNum=4207 AND StreetName LIKE '%VALLEY%'\". Returns PIN + R-account. NOTE: owner & value are NOT in this open layer — take the R-account to the property-search portal below.",
            property_search: "https://www.southwestdatasolution.com/webindex.aspx?dbkey=MIDLANDCAD",
            gis_map: "https://gis.bisclient.com/midlandcad/",
        },
    )]
});

static RE_COUNTY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s+county\b").unwrap());
static RE_COMMA_WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*,\s*").unwrap());

/// `_portal_key` (badbitch2.py:749).
fn portal_key(county_state: &str) -> String {
    let k = RE_COUNTY.replace_all(county_state.trim().to_lowercase().as_str(), "").to_string();
    RE_COMMA_WS.replace_all(&k, ", ").to_string()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindCountyPortalsInput {
    pub county_state: String,
}

#[tool(
    name = "find_county_portals",
    description = "Find the official county appraisal/assessor, tax collector, clerk/recorder, and GIS portals for a 'County, ST'. Returns operator-VERIFIED endpoints first (when known), then web-search candidates. Feed the parcel ArcGIS layer into arcgis_query; use the property search for owner/value. Use this before pulling parcel records."
)]
pub async fn find_county_portals(ctx: ToolContext, input: FindCountyPortalsInput) -> String {
    let cs = &input.county_state;
    let mut out: Vec<String> = Vec::new();
    let key = portal_key(cs);
    if let Some((_, p)) = KNOWN_PORTALS.iter().find(|(k, _)| *k == key) {
        out.push(format!("# VERIFIED portals for {cs} — use these directly, do NOT guess:"));
        out.push(format!("- Appraisal district: {}", p.appraisal_district));
        out.push(format!("- Parcel ArcGIS layer (no key, JSON): {}", p.parcel_arcgis));
        out.push(format!("    fields: {}", p.parcel_fields));
        out.push(format!("    lookup: {}", p.parcel_lookup));
        out.push(format!("- Property search (owner/value/tax/history): {}", p.property_search));
        out.push(format!("- Interactive GIS map: {}\n", p.gis_map));
    }
    out.push(format!("# Web-search candidates for {cs}"));
    let candidates = [
        ("Appraisal/Assessor", format!("{cs} county appraisal district property search")),
        ("Tax / delinquency", format!("{cs} county tax collector delinquent property search")),
        ("Clerk / Recorder", format!("{cs} county clerk recorder deed records online")),
        ("GIS (ArcGIS REST)", format!("{cs} county GIS parcel arcgis rest services MapServer")),
        ("Code enforcement", format!("{cs} code enforcement vacant condemned property list")),
    ];
    for (label, q) in candidates {
        let res = web_search(ctx.clone(), WebSearchInput { query: q, max_results: 4 }).await;
        out.push(format!("\n## {label}\n{res}"));
    }
    out.join("\n")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArcgisQueryInput {
    /// Layer endpoint ending in a number, e.g. https://gis.county.gov/.../MapServer/0
    pub layer_url: String,
    #[serde(default = "default_where", rename = "where")]
    #[schemars(rename = "where")]
    pub where_: String,
    #[serde(default = "default_out_fields")]
    pub out_fields: String,
    #[serde(default)]
    pub geometry: String,
    #[serde(default = "default_max_records")]
    pub max_records: u32,
}
fn default_where() -> String {
    "1=1".to_string()
}
fn default_out_fields() -> String {
    "*".to_string()
}
fn default_max_records() -> u32 {
    10
}

#[tool(
    name = "arcgis_query",
    description = "Query an ArcGIS REST FeatureServer/MapServer layer (most county assessor/GIS portals are ArcGIS and return JSON). layer_url is the layer endpoint ending in a number. `where_` is a SQL filter (e.g. \"SITUS_ADDR LIKE '%MAIN ST%'\"). `geometry` optional 'lon,lat' for point-in-parcel. Returns matching features as JSON. Pair with find_county_portals to locate the layer."
)]
pub async fn arcgis_query(ctx: ToolContext, input: ArcgisQueryInput) -> String {
    let url = format!("{}/query", input.layer_url.trim_end_matches('/'));
    let mut query = vec![
        ("where".to_string(), input.where_.clone()),
        ("outFields".to_string(), input.out_fields.clone()),
        ("f".to_string(), "json".to_string()),
        ("returnGeometry".to_string(), "false".to_string()),
        ("resultRecordCount".to_string(), input.max_records.to_string()),
    ];
    if !input.geometry.is_empty() {
        let parts: Vec<&str> = input.geometry.split(',').map(|s| s.trim()).collect();
        if parts.len() >= 2 {
            query.push(("geometry".to_string(), format!("{},{}", parts[0], parts[1])));
            query.push(("geometryType".to_string(), "esriGeometryPoint".to_string()));
            query.push(("inSR".to_string(), "4326".to_string()));
            query.push(("spatialRel".to_string(), "esriSpatialRelIntersects".to_string()));
        }
    }
    let resp = match http::get(&ctx.http, &ctx.config, &url, &query, &[]).await {
        Ok(r) => r,
        Err(e) => {
            return format!("[arcgis error] {e} — confirm the URL is a layer endpoint ending in a number.");
        }
    };
    let data: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return format!("[arcgis error] {e} — confirm the URL is a layer endpoint ending in a number.");
        }
    };
    let feats = data.get("features").and_then(|f| f.as_array());
    match feats {
        Some(feats) if !feats.is_empty() => {
            let attrs: Vec<Value> = feats
                .iter()
                .map(|f| f.get("attributes").cloned().unwrap_or(Value::Null))
                .collect();
            format!(
                "[{} feature(s)]\n{}",
                attrs.len(),
                http::compact_json(&Value::Array(attrs), 5000)
            )
        }
        _ => http::compact_json(&data, 2500),
    }
}

// ---- Phase 2: structured property APIs ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AttomPropertyInput {
    /// 'street, city ST'
    pub address: String,
}

#[tool(
    name = "attom_property",
    description = "ATTOM Data property detail (owner, APN, year built, sqft, last sale, assessed/market value, mailing vs situs address). address = 'street, city ST'. Returns JSON."
)]
pub async fn attom_property(ctx: ToolContext, input: AttomPropertyInput) -> String {
    let k = ctx.config.key("attom");
    if k.is_empty() {
        return ctx.config.need_key("attom", "ATTOM", "https://api.developer.attomdata.com/");
    }
    let (a1, a2) = match input.address.split_once(',') {
        Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
        None => (input.address.trim().to_string(), String::new()),
    };
    let query = vec![("address1".into(), a1), ("address2".into(), a2)];
    let headers = vec![("apikey".into(), k), ("Accept".into(), "application/json".into())];
    match http::get(
        &ctx.http,
        &ctx.config,
        "https://api.gateway.attomdata.com/propertyapi/v1.0.0/property/detail",
        &query,
        &headers,
    )
    .await
    {
        Ok(r) => http::resp_json_compact(r, 5000).await,
        Err(e) => format!("[attom error] {e}"),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegridParcelInput {
    pub query: String,
}

#[tool(
    name = "regrid_parcel",
    description = "Regrid parcel lookup by address or query (owner, APN, geometry, zoning, acreage). Returns JSON parcel records."
)]
pub async fn regrid_parcel(ctx: ToolContext, input: RegridParcelInput) -> String {
    let k = ctx.config.key("regrid");
    if k.is_empty() {
        return ctx.config.need_key("regrid", "Regrid", "https://regrid.com/api");
    }
    let query = vec![
        ("query".into(), input.query.clone()),
        ("token".into(), k),
        ("limit".into(), "5".into()),
    ];
    match http::get(&ctx.http, &ctx.config, "https://app.regrid.com/api/v1/search.json", &query, &[]).await {
        Ok(r) => http::resp_json_compact(r, 5000).await,
        Err(e) => format!("[regrid error] {e}"),
    }
}
