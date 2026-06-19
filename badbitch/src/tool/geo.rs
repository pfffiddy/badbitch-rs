//! Geospatial tool: geocode (badbitch2.py:548). (imagery_links/suncalc are Phase 2.)

use std::f64::consts::PI;
use std::sync::LazyLock;

use badbitch_macros::tool;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::http;
use crate::tool::ToolContext;

static RE_STATE_ZIP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z]{2}\b|\b\d{5}\b").unwrap());

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GeocodeInput {
    pub address: String,
}

#[tool(
    name = "geocode",
    description = "Resolve a street address to lat/lon + canonical form via OpenStreetMap Nominatim (free, no key, rate-limited to 1/s). Seeds Street View / aerial / parcel lookups."
)]
pub async fn geocode(ctx: ToolContext, input: GeocodeInput) -> String {
    let cfg = &ctx.config;
    http::rate_limit("nominatim", 1.1).await;

    let address = &input.address;
    let mut query = vec![
        ("q".to_string(), address.clone()),
        ("format".to_string(), "json".to_string()),
        ("limit".to_string(), "1".to_string()),
        ("addressdetails".to_string(), "1".to_string()),
    ];
    if !cfg.geocode_cc.is_empty() {
        query.push(("countrycodes".to_string(), cfg.geocode_cc.clone()));
    }
    let headers = vec![(
        "User-Agent".to_string(),
        "badbitch-osint/2.0 (contact: local)".to_string(),
    )];

    let resp = match http::get(&ctx.http, cfg, "https://nominatim.openstreetmap.org/search", &query, &headers).await {
        Ok(r) => r,
        Err(e) => return format!("[geocode error] {e}"),
    };
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return format!("[geocode error] {e}"),
    };

    let thin = !address.contains(',') && !RE_STATE_ZIP.is_match(address);
    let arr = body.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        let hint = if thin && !cfg.geocode_cc.is_empty() {
            format!(
                " — no '{}' match for a bare street. Add city + state, e.g. '{address}, City, ST', and retry.",
                cfg.geocode_cc
            )
        } else {
            String::new()
        };
        return format!("[no geocode match] {address}{hint}");
    }

    let h = &arr[0];
    let cc = h
        .get("address")
        .and_then(|a| a.get("country_code"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_lowercase();
    let warn = if !cfg.geocode_cc.is_empty() && !cc.is_empty() && cc != cfg.geocode_cc.to_lowercase() {
        format!(
            "\n[warning] match resolved to country '{cc}', not '{}' — the address is too thin. Add city + state and retry before trusting this.",
            cfg.geocode_cc
        )
    } else if thin {
        "\n[note] bare street with no city/state — this is a best guess; confirm city/state from another source before pivoting to a parcel lookup.".to_string()
    } else {
        String::new()
    };

    let get_str = |k: &str| h.get(k).and_then(|v| v.as_str()).unwrap_or("");
    format!(
        "lat: {}\nlon: {}\ncanonical: {}\nosm_type: {}/{}{warn}",
        get_str("lat"),
        get_str("lon"),
        get_str("display_name"),
        get_str("type"),
        get_str("class"),
    )
}

// ---- Phase 2: imagery links + sun position ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImageryLinksInput {
    pub lat: String,
    pub lon: String,
}

#[tool(
    name = "imagery_links",
    description = "Build Google Street View / Maps / Earth links for coordinates so the user can eyeball a property and use the Street View time slider to estimate vacancy onset (boarded windows, overgrowth). No API key."
)]
pub async fn imagery_links(_ctx: ToolContext, input: ImageryLinksInput) -> String {
    let (lat, lon) = (&input.lat, &input.lon);
    format!(
        "Street View pano: https://www.google.com/maps/@?api=1&map_action=pano&viewpoint={lat},{lon}\n\
         Street View (alt):  https://maps.google.com/maps?q=&layer=c&cbll={lat},{lon}\n\
         Satellite/Maps:     https://www.google.com/maps/search/?api=1&query={lat},{lon}\n\
         Google Earth web:   https://earth.google.com/web/@{lat},{lon},0a,300d\n\
         Bing Bird's Eye:    https://www.bing.com/maps?cp={lat}~{lon}&style=o&lvl=19\n\
         Tip: in Street View click the date (top-left) to scrub historical captures."
    )
}

fn parse_when(when_iso: &str) -> Option<DateTime<Utc>> {
    if when_iso.is_empty() {
        return Some(Utc::now());
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(when_iso) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(when_iso, "%Y-%m-%dT%H:%M:%S") {
        return Some(DateTime::from_naive_utc_and_offset(ndt, Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(when_iso, "%Y-%m-%d") {
        return Some(DateTime::from_naive_utc_and_offset(d.and_hms_opt(0, 0, 0)?, Utc));
    }
    None
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SuncalcInput {
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub when_iso: String,
}

#[tool(
    name = "suncalc",
    description = "Compute sun position (azimuth/altitude) + sunrise/sunset/solar-noon for given coordinates and time (UTC). Use for CHRONOLOCATION: verify/estimate when a photo was taken from shadow direction & length, or sanity-check a claimed timestamp. when_iso is an ISO datetime (e.g. 2024-07-04T15:30:00); defaults to now."
)]
pub async fn suncalc(_ctx: ToolContext, input: SuncalcInput) -> String {
    let dt = match parse_when(input.when_iso.trim()) {
        Some(d) => d,
        None => return format!("[suncalc error] could not parse when_iso '{}'", input.when_iso),
    };
    let (lat, lon) = (input.lat, input.lon);
    let ts_ms = dt.timestamp() as f64 * 1000.0;

    let rad = PI / 180.0;
    const DAY_MS: f64 = 86_400_000.0;
    const J1970: f64 = 2_440_588.0;
    const J2000: f64 = 2_451_545.0;
    let e = rad * 23.4397;

    let to_days = |ms: f64| (ms / DAY_MS - 0.5 + J1970) - J2000;
    let solar_mean_anomaly = |d: f64| rad * (357.5291 + 0.98560028 * d);
    let ecliptic_longitude = |m: f64| {
        let c = rad * (1.9148 * m.sin() + 0.02 * (2.0 * m).sin() + 0.0003 * (3.0 * m).sin());
        m + c + rad * 102.9372 + PI
    };
    let declination = |l: f64| (l.sin() * e.sin()).asin();
    let right_ascension = |l: f64| (l.sin() * e.cos()).atan2(l.cos());
    let sidereal_time = |d: f64, lw: f64| rad * (280.16 + 360.9856235 * d) - lw;

    let d = to_days(ts_ms);
    let lw = rad * -lon;
    let phi = rad * lat;
    let m = solar_mean_anomaly(d);
    let l = ecliptic_longitude(m);
    let dec = declination(l);
    let ra = right_ascension(l);
    let h = sidereal_time(d, lw) - ra;
    let az = h.sin().atan2(h.cos() * phi.sin() - dec.tan() * phi.cos());
    let alt = (phi.sin() * dec.sin() + phi.cos() * dec.cos() * h.cos()).asin();
    let az_compass = (az / rad + 180.0).rem_euclid(360.0);
    let alt_deg = alt / rad;
    let shadow_bearing = (az_compass + 180.0).rem_euclid(360.0);
    let shadow_ratio = if alt_deg <= 0.0 {
        "infinite (sun at/below horizon)".to_string()
    } else {
        format!("{:.2}x object height", 1.0 / alt.tan())
    };

    // sunrise / sunset / solar noon
    let j0 = 0.0009;
    let n = (d - j0 - lw / (2.0 * PI)).round();
    let ds = j0 + lw / (2.0 * PI) + n;
    let mn = solar_mean_anomaly(ds);
    let ln = ecliptic_longitude(mn);
    let decn = declination(ln);
    let j_noon = J2000 + ds + 0.0053 * mn.sin() - 0.0069 * (2.0 * ln).sin();
    let h0 = -0.833 * rad;

    let from_j = |j: f64| -> String {
        let secs = (j + 0.5 - J1970) * DAY_MS / 1000.0;
        DateTime::<Utc>::from_timestamp(secs as i64, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| "(invalid)".to_string())
    };

    let acos_arg = (h0.sin() - phi.sin() * decn.sin()) / (phi.cos() * decn.cos());
    let (sunrise, sunset) = if acos_arg.abs() > 1.0 {
        ("(polar day/night — no rise/set)".to_string(), "(polar day/night — no rise/set)".to_string())
    } else {
        let w = acos_arg.acos();
        let a = j0 + (w + lw) / (2.0 * PI) + n;
        let j_set = J2000 + a + 0.0053 * mn.sin() - 0.0069 * (2.0 * ln).sin();
        let j_rise = j_noon - (j_set - j_noon);
        (from_j(j_rise), from_j(j_set))
    };
    let solar_noon = from_j(j_noon);

    format!(
        "time (UTC): {}\n\
         sun azimuth (from N, clockwise): {az_compass:.1}°\n\
         sun altitude: {alt_deg:.1}°  ({})\n\
         shadow points toward: {shadow_bearing:.1}°\n\
         shadow length: {shadow_ratio}\n\
         sunrise (UTC): {sunrise}\nsolar noon (UTC): {solar_noon}\nsunset (UTC): {sunset}\n\
         Note: times are UTC — convert to the location's local timezone.",
        dt.to_rfc3339(),
        if alt_deg > 0.0 { "daylight" } else { "below horizon" }
    )
}
