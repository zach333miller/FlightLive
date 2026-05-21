//! adsb.lol — free, no-auth public ADS-B aggregator.
//! Fetches all aircraft within a radius of a fixed point. ~2s latency,
//! ~10k volunteer receivers — meaningfully better coverage than OpenSky's
//! free tier. We use it as the primary positional source, with OpenSky
//! kept as a fallback to fill in country attribution that adsb.lol omits.

use crate::opensky::RawAircraft;
use serde::Deserialize;
use serde_json::Value;

const URL: &str =
    "https://api.adsb.lol/v2/lat/30.07/lon/-90.62/dist/50"; // 50 NM around Garyville

#[derive(Deserialize)]
struct AdsbLolResponse {
    ac: Option<Vec<AircraftRaw>>,
    /// Server time in milliseconds.
    now: Option<i64>,
}

#[derive(Deserialize)]
struct AircraftRaw {
    /// ICAO 24-bit hex.
    hex: String,
    /// Padded callsign (e.g. "UAL215  ").
    flight: Option<String>,
    /// Tail / registration (e.g. "N12345").
    r: Option<String>,
    /// Aircraft type code (e.g. "B738", "E55P").
    t: Option<String>,
    /// Barometric altitude in ft, OR the literal string "ground".
    alt_baro: Option<Value>,
    /// Ground speed, knots.
    gs: Option<f64>,
    /// True track, degrees.
    track: Option<f64>,
    lat: Option<f64>,
    lon: Option<f64>,
    /// Wake category: A1=light, A2=small, A3=large, A5=heavy, A7=rotor, etc.
    category: Option<String>,
    /// Seconds since this aircraft's position was last received.
    seen_pos: Option<f64>,
}

fn convert(a: AircraftRaw) -> Option<RawAircraft> {
    let lat = a.lat?;
    let lon = a.lon?;

    // alt_baro is either a number (feet) or the string "ground".
    let (altitude_m, on_ground) = match &a.alt_baro {
        Some(Value::String(s)) if s == "ground" => (None, true),
        Some(Value::Number(n)) => (n.as_f64().map(|ft| ft / 3.281), false),
        _ => (None, false),
    };

    let velocity_ms = a.gs.map(|kt| kt * 0.5144);
    let heading = a.track;

    // Country attribution: best-effort from registration prefix. The fusion
    // step in main.rs prefers OpenSky's origin_country when available because
    // it covers more countries than this heuristic.
    let origin_country = match a.r.as_deref() {
        Some(r) if r.starts_with('N') => "United States".to_string(),
        Some(r) if r.starts_with("C-") => "Canada".to_string(),
        Some(r) if r.starts_with("XA") || r.starts_with("XB") || r.starts_with("XC") => {
            "Mexico".to_string()
        }
        Some(r) if r.starts_with("G-") => "United Kingdom".to_string(),
        _ => String::new(),
    };

    Some(RawAircraft {
        icao24: a.hex.trim().to_lowercase(),
        callsign: a
            .flight
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty()),
        origin_country,
        longitude: lon,
        latitude: lat,
        altitude_m,
        velocity_ms,
        heading,
        on_ground,
    })
}

/// Fetch a batch from adsb.lol. Returns server timestamp (seconds since epoch)
/// and parsed aircraft list. Filters out aircraft whose position is more
/// than 60 seconds stale.
pub async fn fetch_batch() -> Result<(i64, Vec<RawAircraft>), String> {
    let resp = reqwest::get(URL)
        .await
        .map_err(|e| format!("adsb.lol request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("adsb.lol http {}", resp.status()));
    }
    let parsed: AdsbLolResponse = resp
        .json()
        .await
        .map_err(|e| format!("adsb.lol parse failed: {e}"))?;

    let aircraft: Vec<RawAircraft> = parsed
        .ac
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.seen_pos.map(|s| s < 60.0).unwrap_or(true))
        .filter_map(convert)
        .collect();

    let now_s = parsed.now.unwrap_or(0) / 1000;
    Ok((now_s, aircraft))
}
