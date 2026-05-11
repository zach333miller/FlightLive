# FlightLive

Real-time ADS-B aircraft viewer for the airspace around Garyville, LA and KMSY.

Rust + Axum backend that pulls state vectors from the
[OpenSky Network](https://opensky-network.org/) public API every 10 seconds,
caches them in memory, and broadcasts updates to all connected clients over
WebSocket. React + TypeScript + Mapbox GL frontend renders each aircraft as
a heading-aligned marker, dead-reckons positions between snapshots, and
shows full details (callsign, altitude, ground speed, heading) in a side
panel on click.

The bounding box is roughly the Mississippi River industrial corridor:
KBTR — Marathon Garyville — KMSY.

## Stack

| Layer | Tech |
|---|---|
| Backend | Rust 1.95, Axum 0.7, tokio (full), serde, reqwest, tower-http |
| Frontend | React 18, TypeScript, Vite, Mapbox GL JS |
| Data | [OpenSky Network REST API](https://openskynetwork.github.io/opensky-api/rest.html) |

## Architecture

```
                ┌───────────────────────────────┐
                │  OpenSky Network public API   │
                └───────────────┬───────────────┘
                                │ HTTPS, every 10s
                                ▼
              ┌─────────────────────────────────────┐
              │   Rust backend (Axum, port 3001)    │
              │                                     │
              │   opensky_fetcher (tokio task)      │
              │      ↓ writes                       │
              │   Arc<RwLock<Snapshot>>             │
              │      ↓ reads                ↘       │
              │   GET /api/aircraft     broadcast::Sender
              │                              ↓      │
              │                          GET /ws ───┼──── ws_session per client
              └─────────────────────────────────────┘
                                │
                                │ WebSocket frames
                                ▼
              ┌─────────────────────────────────────┐
              │  React + Mapbox (Vite, port 5173+)  │
              │                                     │
              │  • WS subscriber updates anchor pos │
              │  • requestAnimationFrame loop dead- │
              │    reckons each marker every frame  │
              │  • Side panel + altitude legend     │
              └─────────────────────────────────────┘
```

### Backend patterns

- `Arc<RwLock<Option<Snapshot>>>` shared cache, multiple readers + single writer
- `tokio::sync::broadcast` channel — fan-out from one fetcher to N WebSocket sessions
- `tokio::select!` inside each `ws_session` to multiplex broadcast receive and client messages
- Heterogeneous JSON positional arrays from OpenSky decoded via `Vec<Vec<serde_json::Value>>` with `filter_map` for graceful tolerance of missing fields

### Frontend patterns

- `useRef` to hold mutable non-React state (the Mapbox map, marker map, animation handles)
- Auto-reconnecting WebSocket with exponential placeholder (2s constant for now)
- Dead-reckoning: project lat/lon forward each frame from `velocity_ms` + `heading_deg` + elapsed time since last snapshot
- Marker reconciliation: add for new aircraft, mutate existing in place, drop departed

## Running locally

Prereqs: Rust 1.95+, Node 18+, a Mapbox public access token.

```bash
# 1. Backend
cd backend
cargo run                # http://localhost:3001

# 2. Frontend (new terminal)
cd frontend
cp .env.example .env
# Edit .env and set VITE_MAPBOX_TOKEN=pk.your_token
npm install
npm run dev              # http://localhost:5173 (or first free port)
```

Open the Vite URL in a browser. The map should populate within ~10 seconds
of backend startup (first OpenSky fetch). The badge in the upper-left flips
to `LIVE` once the WebSocket connects.

## Endpoints

| Method | Path | Description |
|---|---|---|
| GET | `/api/health` | `{ok: true}` — liveness probe |
| GET | `/api/aircraft` | Latest cached snapshot, `{time, fetched_at_ms, aircraft: [...]}` |
| GET | `/ws` | WebSocket; server pushes a `Snapshot` JSON frame per fetch |

## License

MIT
