use axum::{http::StatusCode, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_http::cors::{Any, CorsLayer};

// ---------- health ----------

#[derive(Serialize)]
struct Health {
    ok: bool,
}

async fn health() -> Json<Health> {
    Json(Health { ok: true })
}

// ---------- aircraft (OpenSky proxy) ----------

// OpenSky returns each aircraft as a JSON array of mixed types (string, null,
// number, bool...). Rather than fight serde's type system, we deserialize into
// generic `Value`s and pull fields out by index. The schema (positional):
//   0  icao24            5  longitude
//   1  callsign          6  latitude
//   2  origin_country    7  baro_altitude  (meters)
//   3  time_position     8  on_ground
//   4  last_contact      9  velocity        (m/s)
//                       10  true_track     (degrees, heading)
#[derive(Deserialize)]
struct OpenSkyResponse {
    time: i64,
    states: Option<Vec<Vec<Value>>>,
}

#[derive(Serialize, Debug)]
struct Aircraft {
    icao24: String,
    callsign: Option<String>,
    origin_country: String,
    longitude: f64,
    latitude: f64,
    altitude_m: Option<f64>,
    velocity_ms: Option<f64>,
    heading: Option<f64>,
    on_ground: bool,
}

fn state_to_aircraft(s: &[Value]) -> Option<Aircraft> {
    Some(Aircraft {
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

async fn aircraft() -> Result<Json<Vec<Aircraft>>, (StatusCode, String)> {
    // Bounding box around Marathon Garyville / KMSY / KBTR corridor.
    let url = "https://opensky-network.org/api/states/all\
        ?lamin=29.7&lomin=-91.0&lamax=30.5&lomax=-90.0";

    let resp = reqwest::get(url)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("opensky request failed: {e}")))?
        .json::<OpenSkyResponse>()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("opensky parse failed: {e}")))?;

    let aircraft: Vec<Aircraft> = resp
        .states
        .unwrap_or_default()
        .iter()
        .filter_map(|s| state_to_aircraft(s))
        .collect();

    tracing::info!(
        "opensky returned {} aircraft (time={})",
        aircraft.len(),
        resp.time
    );
    Ok(Json(aircraft))
}

// ---------- main ----------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any);

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/aircraft", get(aircraft))
        .layer(cors);

    let addr = "0.0.0.0:3001";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("FlightLive backend listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}
