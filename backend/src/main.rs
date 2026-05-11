use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::{Any, CorsLayer};

// ---------- domain types ----------

#[derive(Serialize, Debug, Clone)]
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
    time_position: Option<i64>,
}

#[derive(Serialize, Clone)]
struct Snapshot {
    time: i64,
    fetched_at_ms: u128,
    aircraft: Vec<Aircraft>,
}

#[derive(Deserialize)]
struct OpenSkyResponse {
    time: i64,
    states: Option<Vec<Vec<Value>>>,
}

// ---------- shared app state ----------
//
// Cloned cheaply (Arcs inside). Stored once and passed to every handler.
//   cache: latest snapshot — handlers read it without re-fetching OpenSky.
//   tx:    broadcast channel — background fetcher publishes new snapshots,
//          each connected WebSocket client subscribes via tx.subscribe().
#[derive(Clone)]
struct AppState {
    cache: Arc<RwLock<Option<Snapshot>>>,
    tx: broadcast::Sender<Snapshot>,
}

// ---------- OpenSky parsing ----------

fn state_to_aircraft(s: &[Value]) -> Option<Aircraft> {
    Some(Aircraft {
        icao24: s.get(0)?.as_str()?.trim().to_string(),
        callsign: s
            .get(1)
            .and_then(|v| v.as_str())
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty()),
        origin_country: s.get(2)?.as_str()?.to_string(),
        time_position: s.get(3).and_then(|v| v.as_i64()),
        longitude: s.get(5)?.as_f64()?,
        latitude: s.get(6)?.as_f64()?,
        altitude_m: s.get(7).and_then(|v| v.as_f64()),
        on_ground: s.get(8).and_then(|v| v.as_bool()).unwrap_or(false),
        velocity_ms: s.get(9).and_then(|v| v.as_f64()),
        heading: s.get(10).and_then(|v| v.as_f64()),
    })
}

async fn fetch_opensky() -> Result<Snapshot, String> {
    let url = "https://opensky-network.org/api/states/all\
        ?lamin=29.7&lomin=-91.0&lamax=30.5&lomax=-90.0";

    let resp: OpenSkyResponse = reqwest::get(url)
        .await
        .map_err(|e| format!("request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("parse failed: {e}"))?;

    let aircraft: Vec<Aircraft> = resp
        .states
        .unwrap_or_default()
        .iter()
        .filter_map(|s| state_to_aircraft(s))
        .collect();

    let fetched_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();

    Ok(Snapshot {
        time: resp.time,
        fetched_at_ms,
        aircraft,
    })
}

// ---------- background fetcher ----------
//
// Spawned once in main(). Ticks every 10s (OpenSky anonymous rate limit),
// writes the new Snapshot into the shared cache, and broadcasts it to all
// connected WS clients via the broadcast channel.
async fn opensky_fetcher(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;
        match fetch_opensky().await {
            Ok(snapshot) => {
                tracing::info!(
                    "fetched {} aircraft (opensky time={})",
                    snapshot.aircraft.len(),
                    snapshot.time
                );
                *state.cache.write().await = Some(snapshot.clone());
                // send returns Err only when zero subscribers — fine, ignore.
                let _ = state.tx.send(snapshot);
            }
            Err(e) => tracing::warn!("opensky fetch failed: {e}"),
        }
    }
}

// ---------- HTTP handlers ----------

#[derive(Serialize)]
struct Health {
    ok: bool,
}

async fn health() -> Json<Health> {
    Json(Health { ok: true })
}

async fn aircraft(
    State(state): State<AppState>,
) -> Result<Json<Snapshot>, (StatusCode, String)> {
    let cache = state.cache.read().await;
    match &*cache {
        Some(snap) => Ok(Json(snap.clone())),
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no data yet — first OpenSky fetch pending".to_string(),
        )),
    }
}

// ---------- WebSocket handler ----------

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_session(socket, state))
}

// One task per connected client.
// Pushes the current cached snapshot immediately so the map populates on
// connect, then forwards every broadcast to the socket until the client
// disconnects or the channel closes.
async fn ws_session(mut socket: WebSocket, state: AppState) {
    if let Some(snap) = state.cache.read().await.clone() {
        if let Ok(json) = serde_json::to_string(&snap) {
            if socket.send(Message::Text(json)).await.is_err() {
                return;
            }
        }
    }

    let mut rx = state.tx.subscribe();

    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(snap) => {
                    let json = match serde_json::to_string(&snap) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if socket.send(Message::Text(json)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("ws client lagged, dropped {n} messages");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {} // ignore pings/pongs/text from client
            },
        }
    }
}

// ---------- main ----------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // capacity = 16: if a slow client is more than 16 snapshots behind,
    // it gets a Lagged error (we just continue past it).
    let (tx, _) = broadcast::channel::<Snapshot>(16);

    let state = AppState {
        cache: Arc::new(RwLock::new(None)),
        tx,
    };

    tokio::spawn(opensky_fetcher(state.clone()));

    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any);

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/aircraft", get(aircraft))
        .route("/ws", get(ws_handler))
        .layer(cors)
        .with_state(state);

    let addr = "0.0.0.0:3001";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("FlightLive backend listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}
