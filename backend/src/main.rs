mod adsb_lol;
mod analysis;
mod opensky;
mod types;
mod weather;
// Narrator module retained in git history; not loaded at runtime since the
// pivot to the pre-flight check framing. Resurrect via `mod narrator;` if
// you want the LLM commentary back.

use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::{Any, CorsLayer};

/// Embed the compiled React bundle into the binary at build time.
/// Path is relative to this crate's Cargo.toml.
#[derive(RustEmbed)]
#[folder = "../frontend/dist/"]
struct Asset;

use crate::analysis::{detect_conflicts, predict_audible, ACOUSTIC_HORIZON_S};
use crate::opensky::{fetch_batch, make_track_point, now_ms, now_ms_u64, OpenSkyAuth};
use crate::types::*;
use crate::weather::weather_task;

const WEATHER_STATION: &str = "KAPS"; // Reserve / Port of South Louisiana Exec — next to Marathon Garyville

#[derive(Clone)]
struct AppState {
    cache: Arc<RwLock<Option<Snapshot>>>,
    history: Arc<RwLock<HistoryMap>>,
    weather: Arc<RwLock<Option<Weather>>>,
    snap_tx: broadcast::Sender<Snapshot>,
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

// ---------- Static file handler (serves the embedded React bundle) ----------
//
// Mounted as the Router's fallback so any non-API path resolves to either a
// real asset (hashed JS/CSS/img) or — for SPA routes the React router owns —
// the index.html shell. This is what turns the binary into a standalone .exe.
async fn static_handler(uri: Uri) -> impl IntoResponse {
    let raw = uri.path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };
    serve_embedded(path)
}

fn serve_embedded(path: &str) -> Response {
    match Asset::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(file.data.into_owned()))
                .unwrap()
        }
        // Unknown path → serve index.html so client-side routing still works.
        None => match Asset::get("index.html") {
            Some(file) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(file.data.into_owned()))
                .unwrap(),
            None => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from(
                    "frontend bundle not embedded — did you `npm run build` before `cargo build`?",
                ))
                .unwrap(),
        },
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

        // Fan out to both sources in parallel — adsb.lol (primary: fresh
        // positions, ~10k receivers) and OpenSky (fallback: weaker coverage
        // but provides origin_country attribution). The union is broader
        // than either feed alone.
        let (adsb_result, opensky_result) = tokio::join!(
            adsb_lol::fetch_batch(),
            fetch_batch(state.opensky_auth.as_ref()),
        );

        // Surface failures separately — only give up the whole tick if both
        // sources errored. A single dead source isn't a tick we want to lose.
        let adsb_ok = adsb_result.is_ok();
        let opensky_ok = opensky_result.is_ok();
        if !adsb_ok && !opensky_ok {
            if let Err(e) = &adsb_result {
                tracing::warn!("adsb.lol fetch failed: {e}");
            }
            if let Err(e) = &opensky_result {
                if e.contains("429") {
                    backoff = (backoff.saturating_mul(2)).min(max_secs);
                    tracing::warn!(
                        "opensky rate-limited, backing off to {backoff}s"
                    );
                } else {
                    tracing::warn!("opensky fetch failed: {e}");
                }
            }
            continue;
        }
        backoff = base_secs;

        // Fuse: start from OpenSky for country attribution, then overlay
        // adsb.lol's fresher position/heading/altitude. Anything unique to
        // either source is included.
        use std::collections::HashMap;
        let mut combined: HashMap<String, crate::opensky::RawAircraft> = HashMap::new();
        let (opensky_time, opensky_count) = match &opensky_result {
            Ok((t, raws)) => {
                for r in raws {
                    combined.insert(r.icao24.clone(), r.clone());
                }
                (*t, raws.len())
            }
            Err(_) => (0, 0),
        };
        let (adsb_time, adsb_count) = match &adsb_result {
            Ok((t, raws)) => {
                for r in raws {
                    match combined.get_mut(&r.icao24) {
                        Some(existing) => {
                            // adsb.lol position wins (fresher).
                            existing.longitude = r.longitude;
                            existing.latitude = r.latitude;
                            existing.altitude_m = r.altitude_m;
                            existing.velocity_ms = r.velocity_ms;
                            existing.heading = r.heading;
                            existing.on_ground = r.on_ground;
                            if existing.callsign.is_none() {
                                existing.callsign = r.callsign.clone();
                            }
                            // Keep OpenSky's origin_country if present, else
                            // take adsb.lol's registration-derived one.
                            if existing.origin_country.is_empty() {
                                existing.origin_country = r.origin_country.clone();
                            }
                        }
                        None => {
                            combined.insert(r.icao24.clone(), r.clone());
                        }
                    }
                }
                (*t, raws.len())
            }
            Err(_) => (0, 0),
        };
        let raws: Vec<crate::opensky::RawAircraft> = combined.into_values().collect();
        // Use whichever source provided the more recent timestamp.
        let opensky_time = opensky_time.max(adsb_time);

        tracing::info!(
            "fused snapshot — adsb.lol={} opensky={} union={}",
            adsb_count,
            opensky_count,
            raws.len()
        );

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
        let weather = state.weather.read().await.clone();

        let snapshot = Snapshot {
            time: opensky_time,
            fetched_at_ms: now_ms(),
            aircraft,
            conflicts,
            audible,
            listener: [LISTENER_LNG, LISTENER_LAT],
            weather,
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
        weather: Arc::new(RwLock::new(None)),
        snap_tx,
        opensky_auth,
    };

    tokio::spawn(fetcher_task(state.clone()));
    tokio::spawn(weather_task(state.weather.clone(), WEATHER_STATION.to_string()));

    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any);

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/aircraft", get(aircraft))
        .route("/ws", get(ws_snap))
        // Everything else falls through to the embedded React bundle — this
        // is what makes the binary self-contained.
        .fallback(static_handler)
        .layer(cors)
        .with_state(state);

    // Use 127.0.0.1 (loopback only) for the .exe — anyone who downloads the
    // file probably doesn't want their tracker exposed on their LAN by
    // default. Port 3001 stays the same so deep-linked screenshots work.
    let addr = "127.0.0.1:3001";
    let url = format!("http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("FlightLive listening on {url}");

    // Best-effort: open the user's default browser as soon as we're bound.
    // Failing this isn't fatal — print a fallback URL.
    if webbrowser::open(&url).is_err() {
        eprintln!("\n  >> open {url} in your browser to view FlightLive\n");
    }

    axum::serve(listener, app).await.unwrap();
}
