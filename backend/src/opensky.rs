//! OpenSky API fetch + parsing.
//!
//! Authentication: OpenSky migrated to OAuth2 Client Credentials in 2024-2025.
//! Old user/password Basic Auth is gone. The flow is:
//!   1. POST to the Keycloak token endpoint with client_id + client_secret
//!   2. Receive a Bearer access_token valid for ~30 min
//!   3. Send `Authorization: Bearer <token>` on each /api/states/all call
//!   4. Refresh proactively before expiry
//! Anonymous (no auth) works but caps at ~100 req/day; authenticated default
//! gets 4,000 credits/day per the account dashboard.

use crate::types::*;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

const TOKEN_URL: &str = "https://auth.opensky-network.org\
    /auth/realms/opensky-network/protocol/openid-connect/token";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Seconds until the token expires (typically 1800 = 30 min).
    expires_in: u64,
}

/// OAuth2 client-credentials holder + cached bearer token.
/// `Arc` it and clone the Arc to share across tasks.
pub struct OpenSkyAuth {
    client_id: String,
    client_secret: String,
    /// (token, expires_at). `None` = not yet fetched.
    cached: RwLock<Option<(String, Instant)>>,
}

impl OpenSkyAuth {
    /// Build from env vars. Returns `None` if either OPENSKY_CLIENT_ID
    /// or OPENSKY_CLIENT_SECRET is missing — we then fall back to anonymous.
    pub fn from_env() -> Option<Arc<Self>> {
        let id = std::env::var("OPENSKY_CLIENT_ID").ok()?;
        let secret = std::env::var("OPENSKY_CLIENT_SECRET").ok()?;
        if id.is_empty() || secret.is_empty() {
            return None;
        }
        Some(Arc::new(Self {
            client_id: id,
            client_secret: secret,
            cached: RwLock::new(None),
        }))
    }

    /// Get a valid bearer token, fetching/refreshing if needed.
    /// Refresh 30 s before expiry so we never present a stale token.
    pub async fn token(&self, http: &reqwest::Client) -> Result<String, String> {
        {
            let cache = self.cached.read().await;
            if let Some((tok, expires_at)) = &*cache {
                if Instant::now() + Duration::from_secs(30) < *expires_at {
                    return Ok(tok.clone());
                }
            }
        }
        let resp: TokenResponse = http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("token request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("token rejected: {e}"))?
            .json()
            .await
            .map_err(|e| format!("token parse failed: {e}"))?;

        let expires_at = Instant::now() + Duration::from_secs(resp.expires_in.saturating_sub(30));
        let mut cache = self.cached.write().await;
        *cache = Some((resp.access_token.clone(), expires_at));
        Ok(resp.access_token)
    }
}

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
/// Pass `Some(auth)` to use OAuth2 (4,000 credits/day); pass `None` for the
/// anonymous tier (~100 credits/day, fine for development but exhausts fast).
pub async fn fetch_batch(
    auth: Option<&Arc<OpenSkyAuth>>,
) -> Result<(i64, Vec<RawAircraft>), String> {
    let url = "https://opensky-network.org/api/states/all\
        ?lamin=29.7&lomin=-91.0&lamax=30.5&lomax=-90.0";

    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if let Some(a) = auth {
        let token = a.token(&client).await?;
        req = req.bearer_auth(token);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
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
