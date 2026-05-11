//! OpenSky API fetch + parsing.

use crate::types::*;
use serde::Deserialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
pub struct OpenSkyResponse {
    pub time: i64,
    pub states: Option<Vec<Vec<Value>>>,
}

/// Decode a single OpenSky positional array into our cleaned shape.
/// Returns `None` if any required field (icao24, lat, lng) is missing.
pub fn state_to_partial(s: &[Value]) -> Option<RawAircraft> {
    Some(RawAircraft {
        icao24: s.get(0)?.as_str()?.trim().to_string(),
        callsign: s
            .get(1)
            .and_then(|v| v.as_str())
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty()),
        origin_country: s.get(2)?.as_str()?.to_string(),
        longitude: s.get(5)?.as_f64()?,
        latitude: s.get(6)?.as_f64()?,
        altitude_m: s.get(7).and_then(|v| v.as_f64()),
        on_ground: s.get(8).and_then(|v| v.as_bool()).unwrap_or(false),
        velocity_ms: s.get(9).and_then(|v| v.as_f64()),
        heading: s.get(10).and_then(|v| v.as_f64()),
    })
}

/// Pre-classifier aircraft — same shape as Aircraft but no behavior/trail yet.
#[derive(Debug, Clone)]
pub struct RawAircraft {
    pub icao24: String,
    pub callsign: Option<String>,
    pub origin_country: String,
    pub longitude: f64,
    pub latitude: f64,
    pub altitude_m: Option<f64>,
    pub velocity_ms: Option<f64>,
    pub heading: Option<f64>,
    pub on_ground: bool,
}

/// Fetch a single batch from OpenSky. Returns the snapshot time (seconds since
/// epoch) and parsed RawAircraft list.
///
/// Reads OPENSKY_USERNAME / OPENSKY_PASSWORD from env if set — authenticated
/// accounts get ~4,000 credits/day vs ~100 for anonymous.
pub async fn fetch_batch() -> Result<(i64, Vec<RawAircraft>), String> {
    let url = "https://opensky-network.org/api/states/all\
        ?lamin=29.7&lomin=-91.0&lamax=30.5&lomax=-90.0";

    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if let (Ok(u), Ok(p)) = (std::env::var("OPENSKY_USERNAME"), std::env::var("OPENSKY_PASSWORD"))
    {
        req = req.basic_auth(u, Some(p));
    }

    let resp = req.send().await.map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(80).collect();
        // Tag rate-limit errors so the caller can apply backoff.
        if status.as_u16() == 429 {
            return Err(format!("opensky http 429 rate limited: {snippet}"));
        }
        return Err(format!("opensky http {status}: {snippet}"));
    }

    let parsed: OpenSkyResponse = resp
        .json()
        .await
        .map_err(|e| format!("parse failed: {e}"))?;

    let aircraft: Vec<RawAircraft> = parsed
        .states
        .unwrap_or_default()
        .iter()
        .filter_map(|s| state_to_partial(s))
        .collect();

    Ok((parsed.time, aircraft))
}

pub fn now_ms_u64() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

/// Build a TrackPoint for the history buffer from a raw aircraft observation.
pub fn make_track_point(a: &RawAircraft, time_ms: u64) -> TrackPoint {
    TrackPoint {
        time_ms,
        lng: a.longitude,
        lat: a.latitude,
        altitude_m: a.altitude_m,
        velocity_ms: a.velocity_ms,
        heading: a.heading,
        on_ground: a.on_ground,
    }
}
