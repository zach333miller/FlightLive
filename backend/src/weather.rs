//! METAR fetcher.
//!
//! Pulls the latest observation for a fixed station (KMSY) from the FAA's
//! free aviationweather.gov API. Updated every ~5 minutes by a background
//! task. The result is attached to every aircraft Snapshot so the frontend
//! always has fresh weather paired with fresh airspace data.

use crate::types::*;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

#[derive(Deserialize)]
struct MetarRaw {
    #[serde(rename = "icaoId")]
    icao_id: String,
    #[serde(rename = "obsTime")]
    obs_time: i64,
    temp: Option<f64>,
    dewp: Option<f64>,
    /// Number (degrees) or string ("VRB"). Use Value to tolerate both.
    wdir: Option<serde_json::Value>,
    wspd: Option<f64>,
    wgst: Option<f64>,
    /// Number (statute miles) or string ("10+"). Same trick.
    visib: Option<serde_json::Value>,
    altim: Option<f64>,
    #[serde(rename = "wxString")]
    wx_string: Option<String>,
    clouds: Option<Vec<CloudLayer>>,
    #[serde(rename = "fltCat")]
    flt_cat: Option<String>,
    #[serde(rename = "rawOb")]
    raw_ob: String,
}

#[derive(Deserialize, Debug, Clone)]
struct CloudLayer {
    cover: String,
    base: Option<i32>,
}

/// Ceiling is the lowest BKN or OVC layer, in feet AGL.
fn compute_ceiling_ft(clouds: &Option<Vec<CloudLayer>>) -> Option<i32> {
    clouds
        .as_ref()?
        .iter()
        .filter(|c| c.cover == "BKN" || c.cover == "OVC")
        .filter_map(|c| c.base)
        .min()
}

fn value_as_i32(v: &Option<serde_json::Value>) -> Option<i32> {
    v.as_ref()?.as_i64().map(|n| n as i32)
}

fn value_as_visib_string(v: &Option<serde_json::Value>) -> Option<String> {
    let v = v.as_ref()?;
    match v {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn raw_to_weather(raw: MetarRaw) -> Weather {
    let ceiling_ft = compute_ceiling_ft(&raw.clouds);
    Weather {
        station: raw.icao_id,
        observed_at_s: raw.obs_time,
        temp_c: raw.temp,
        dewpoint_c: raw.dewp,
        wind_dir_deg: value_as_i32(&raw.wdir),
        wind_speed_kt: raw.wspd,
        wind_gust_kt: raw.wgst,
        visibility_sm: value_as_visib_string(&raw.visib),
        altimeter_hpa: raw.altim,
        ceiling_ft,
        flight_category: raw.flt_cat,
        wx_string: raw.wx_string,
        raw: raw.raw_ob,
    }
}

pub async fn fetch_metar(station: &str) -> Result<Weather, String> {
    let url = format!(
        "https://aviationweather.gov/api/data/metar?ids={}&format=json",
        station
    );
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("metar request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("metar http {}", resp.status()));
    }
    let arr: Vec<MetarRaw> = resp
        .json()
        .await
        .map_err(|e| format!("metar parse failed: {e}"))?;
    arr.into_iter()
        .next()
        .map(raw_to_weather)
        .ok_or_else(|| "metar response had no observations".to_string())
}

/// Background task: refresh METAR every 5 minutes into a shared cell.
pub async fn weather_task(weather: Arc<RwLock<Option<Weather>>>, station: String) {
    // Quick initial fetch, then a slow loop.
    if let Ok(w) = fetch_metar(&station).await {
        tracing::info!("metar: {} {}", w.station, w.flight_category.clone().unwrap_or_default());
        *weather.write().await = Some(w);
    }
    let mut interval = tokio::time::interval(Duration::from_secs(5 * 60));
    interval.tick().await; // skip the immediate fire
    loop {
        interval.tick().await;
        match fetch_metar(&station).await {
            Ok(w) => {
                tracing::info!("metar: {} {}", w.station, w.flight_category.clone().unwrap_or_default());
                *weather.write().await = Some(w);
            }
            Err(e) => tracing::warn!("metar fetch failed: {e}"),
        }
    }
}
