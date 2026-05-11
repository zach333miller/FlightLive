mod analysis;
mod narrator;
mod opensky;
mod types;

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
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::{Any, CorsLayer};

use crate::analysis::{detect_conflicts, predict_audible, ACOUSTIC_HORIZON_S};
use crate::narrator::narrator_task;
use crate::opensky::{fetch_batch, make_track_point, now_ms, now_ms_u64, OpenSkyAuth};
use crate::types::*;

#[derive(Clone)]
struct AppState {
    cache: Arc<RwLock<Option<Snapshot>>>,
    history: Arc<RwLock<HistoryMap>>,
    snap_tx: broadcast::Sender<Snapshot>,
    narr_tx: broadcast::Sender<Narration>,
    recent_narrations: Arc<RwLock<VecDeque<String>>>,
    opensky_auth: Option<Arc<OpenSkyAuth>>,
}

// ---------- HTTP handlers ----------

#[derive(Serialize)]
struct Health {
    ok: bool,
}

async fn health() -> Json<Health> {
    Json(Health { ok: true })
}

async fn aircraft(State(s): State<AppState>) -> Result<Json<Snapshot>, (StatusCode, String)> {
    match s.cache.read().await.clone() {
        Some(snap) => Ok(Json(snap)),
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no data yet — first OpenSky fetch pending".to_string(),
        )),
    }
}

// ---------- WebSocket: aircraft snapshots ----------

async fn ws_snap(ws: WebSocketUpgrade, State(s): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_snap_session(socket, s))
}

async fn ws_snap_session(mut socket: WebSocket, state: AppState) {
    if let Some(snap) = state.cache.read().await.clone() {
        if let Ok(json) = serde_json::to_string(&snap) {
            if socket.send(Message::Text(json)).await.is_err() {
                return;
            }
        }
    }
    let mut rx = state.snap_tx.subscribe();
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(snap) => {
                    let Ok(json) = serde_json::to_string(&snap) else { continue };
                    if socket.send(Message::Text(json)).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("snap ws lagged {n}");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
    }
}

// ---------- WebSocket: narrator stream ----------

async fn ws_narr(ws: WebSocketUpgrade, State(s): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_narr_session(socket, s))
}

async fn ws_narr_session(mut socket: WebSocket, state: AppState) {
    let mut rx = state.narr_tx.subscribe();
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(narr) => {
                    let Ok(json) = serde_json::to_string(&narr) else { continue };
                    if socket.send(Message::Text(json)).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
    }
}

// ---------- Background fetcher + analyzer ----------

async fn fetcher_task(state: AppState) {
    // Sleep between fetches. Starts at the OpenSky-recommended 10 s minimum
    // and doubles on rate-limit (429) errors, capping at 5 minutes. Resets to
    // 10 s on a successful fetch.
    let base_secs: u64 = 10;
    let max_secs: u64 = 300;
    let mut backoff: u64 = base_secs;

    loop {
        tokio::time::sleep(Duration::from_secs(backoff)).await;

        let (opensky_time, raws) = match fetch_batch(state.opensky_auth.as_ref()).await {
            Ok(x) => {
                backoff = base_secs;
                x
            }
            Err(e) => {
                if e.contains("429") {
                    backoff = (backoff.saturating_mul(2)).min(max_secs);
                    tracing::warn!(
                        "opensky rate-limited, backing off to {backoff}s — set OPENSKY_USERNAME / OPENSKY_PASSWORD to raise daily quota"
                    );
                } else {
                    tracing::warn!("opensky fetch failed: {e}");
                }
                continue;
            }
        };

        let ts_ms = now_ms_u64();

        // Update history (write lock, briefly).
        {
            let mut hist = state.history.write().await;
            // Push current observations.
            for raw in &raws {
                let entry = hist.entry(raw.icao24.clone()).or_default();
                entry.push_back(make_track_point(raw, ts_ms));
                while entry.len() > HISTORY_MAX {
                    entry.pop_front();
                }
            }
            // Drop history for aircraft no longer in the box.
            let present: std::collections::HashSet<&str> =
                raws.iter().map(|r| r.icao24.as_str()).collect();
            hist.retain(|k, _| present.contains(k.as_str()));
        }

        // Build aircraft list (classify + attach trail) under a read lock.
        let aircraft: Vec<Aircraft> = {
            let hist = state.history.read().await;
            raws.into_iter()
                .map(|r| {
                    let h = hist.get(&r.icao24);
                    let behavior = match h {
                        Some(buf) => analysis::classify(buf),
                        None => Behavior::Enroute,
                    };
                    let trail: Vec<[f64; 2]> = match h {
                        Some(buf) => buf.iter().map(|p| [p.lng, p.lat]).collect(),
                        None => vec![[r.longitude, r.latitude]],
                    };
                    Aircraft {
                        icao24: r.icao24,
                        callsign: r.callsign,
                        origin_country: r.origin_country,
                        longitude: r.longitude,
                        latitude: r.latitude,
                        altitude_m: r.altitude_m,
                        velocity_ms: r.velocity_ms,
                        heading: r.heading,
                        on_ground: r.on_ground,
                        behavior,
                        trail,
                    }
                })
                .collect()
        };

        let conflicts = detect_conflicts(&aircraft, 60.0);
        let audible = predict_audible(&aircraft, LISTENER_LNG, LISTENER_LAT, ACOUSTIC_HORIZON_S);

        let snapshot = Snapshot {
            time: opensky_time,
            fetched_at_ms: now_ms(),
            aircraft,
            conflicts,
            audible,
            listener: [LISTENER_LNG, LISTENER_LAT],
        };

        tracing::info!(
            "snapshot: {} aircraft, {} conflicts, {} audible upcoming",
            snapshot.aircraft.len(),
            snapshot.conflicts.len(),
            snapshot.audible.len()
        );

        *state.cache.write().await = Some(snapshot.clone());
        let _ = state.snap_tx.send(snapshot);
    }
}

// ---------- main ----------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let (snap_tx, _) = broadcast::channel::<Snapshot>(16);
    let (narr_tx, _) = broadcast::channel::<Narration>(16);

    let opensky_auth = OpenSkyAuth::from_env();
    match &opensky_auth {
        Some(_) => tracing::info!("OpenSky OAuth2 client credentials loaded"),
        None => tracing::warn!(
            "no OPENSKY_CLIENT_ID/SECRET — falling back to anonymous tier (~100 req/day)"
        ),
    }

    let state = AppState {
        cache: Arc::new(RwLock::new(None)),
        history: Arc::new(RwLock::new(HistoryMap::default())),
        snap_tx,
        narr_tx,
        recent_narrations: Arc::new(RwLock::new(VecDeque::new())),
        opensky_auth,
    };

    tokio::spawn(fetcher_task(state.clone()));

    // Narrator: read from env so it's overrideable; defaults to local Ollama.
    let ollama_url =
        std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
    let ollama_model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.1:8b".to_string());
    tokio::spawn(narrator_task(
        state.cache.clone(),
        state.narr_tx.clone(),
        state.recent_narrations.clone(),
        ollama_url,
        ollama_model,
    ));

    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any);

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/aircraft", get(aircraft))
        .route("/ws", get(ws_snap))
        .route("/ws/narration", get(ws_narr))
        .layer(cors)
        .with_state(state);

    let addr = "0.0.0.0:3001";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("FlightLive backend listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}
