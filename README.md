# FlightLive

Real-time ADS-B airspace viewer for the Mississippi River industrial corridor
(Baton Rouge — Marathon Garyville — Louis Armstrong New Orleans International).

Not just a tracker. The Rust backend layers four kinds of inference on top of
the raw OpenSky state vectors:

- **Behavioral classifier** — labels each aircraft as
  `CRUISE / APPROACH / HOLDING / HOVERING / CLIMBING / DESCENDING / TAXIING / ENROUTE`
  by examining its recent trajectory.
- **Conflict detection** — pairwise O(n²) scan at t=0 and at t=60 s
  (after dead-reckoning each aircraft forward) for any pair predicted to be
  within 3 NM horizontal and 1,000 ft vertical separation. Flags the violating
  pair on the map.
- **Acoustic path predictor** — for the Marathon Garyville refinery, projects
  every airborne aircraft forward ~4 min, finds the closest-approach moment,
  estimates dB at the listener using slant-distance attenuation from a per-
  class source level (jet/GA/helicopter), and reports the upcoming audible
  events.
- **LLM airspace narrator** — every 30 s, a `tokio` task builds a structured
  prompt summarizing the airspace state (counts by behavior, notable aircraft,
  active conflicts, audibility events) plus the model's own recent outputs,
  POSTs it to a local Ollama (`llama3.1:8b`), and broadcasts the resulting
  2-3 sentence commentary to every connected WebSocket client.

The frontend then draws:

- Aircraft as heading-aligned, altitude-colored markers
- Flight trails as polylines from each aircraft's server-side history buffer
- The refinery polygon + 5 NM drone-ops ring (Zach's actual LAANC work area)
- Live precipitation from RainViewer beneath the aircraft
- A side panel with the behavior tag, altitude/speed in metric *and* imperial
- An acoustic ticker showing the next 3 audible-at-refinery events
- A live narration feed in the bottom-right

Aircraft positions glide between snapshots using a `requestAnimationFrame`
dead-reckoning loop on the client.

## Stack

| Layer | Tech |
|---|---|
| Backend | Rust 1.95 · Axum 0.7 (with `ws`) · `tokio` (full) · `serde` · `reqwest` · `tower-http` |
| Frontend | React 18 · TypeScript · Vite · Mapbox GL JS |
| LLM | Local Ollama HTTP API · `llama3.1:8b` (overridable via env vars) |
| Data | [OpenSky Network](https://openskynetwork.github.io/opensky-api/rest.html) · [RainViewer radar tiles](https://www.rainviewer.com/api.html) |

## Architecture

```
                ┌────────────────────────────────────────┐
                │   OpenSky public API · every 10 s      │
                └───────────────────┬────────────────────┘
                                    │ HTTPS
                                    ▼
              ┌────────────────────────────────────────────────┐
              │  Rust backend (port 3001)                      │
              │                                                │
              │  fetcher_task  (10s tick)                      │
              │     ├─ updates  Arc<RwLock<HistoryMap>>        │
              │     ├─ classifies  Behavior per aircraft       │
              │     ├─ detects     conflicts (t=0 & t=60s)     │
              │     ├─ predicts    audible-at-refinery events  │
              │     ├─ writes →    Arc<RwLock<Option<Snapshot>>│
              │     └─ broadcasts → tokio::sync::broadcast     │
              │                                                │
              │  narrator_task (30s tick, 15s warm-up)         │
              │     ├─ reads cache + recent narrations         │
              │     ├─ POST   →  Ollama llama3.1:8b            │
              │     └─ broadcasts → narration channel          │
              │                                                │
              │  HTTP  /api/aircraft       (reads cache)       │
              │  WS    /ws                 (snapshot stream)   │
              │  WS    /ws/narration       (narration stream)  │
              └───────────────────────────────────────────────┘
                                    │ WS frames
                                    ▼
              ┌────────────────────────────────────────────────┐
              │  React + Mapbox (port 5173+, via Vite proxy)   │
              │                                                │
              │  ws snapshot     → marker + trail + conflict   │
              │                    + audibility ticker updates │
              │  ws narration    → news ticker                 │
              │  requestAnimationFrame → dead-reckon markers   │
              │  RainViewer tiles ↔ raster source              │
              └────────────────────────────────────────────────┘
```

## Patterns showcased

### Rust

- `Arc<RwLock<…>>` shared state with many readers + occasional writer
- `tokio::sync::broadcast` fan-out from one producer to N WebSocket sessions
- `tokio::select!` to multiplex broadcast receive with client disconnect
- `tokio::spawn` for the fetcher and narrator background tasks
- `Result<T, E>` with the `?` operator and `map_err` for clean error chains
- `serde_json::Value` + `filter_map` for tolerant decode of OpenSky's
  heterogeneous positional arrays
- `VecDeque<TrackPoint>` ring buffer per aircraft, bounded at module level
- Pure-function spatial math (haversine, dead-reckoning) — unit-testable
- Module split: `types` / `opensky` / `analysis` / `narrator` / `main`

### Frontend

- WebSocket subscriber with auto-reconnect
- Mutable non-React state in `useRef` (Mapbox map, marker map, animation handle)
- Mapbox GL JS sources: `geojson` for trails / refinery / conflicts and
  `raster` for weather
- `requestAnimationFrame` dead-reckoning loop runs at ~60 Hz over ~30 markers
- Behavior-aware visual styling (badge color, marker color band)

## Running locally

Prereqs:

- Rust 1.95+
- Node 18+
- A Mapbox public access token
- (Optional) [Ollama](https://ollama.com) running locally with `llama3.1:8b` pulled
  — the narrator gracefully no-ops if the model is unreachable.

```bash
# 1. Backend
cd backend
cargo run                # http://localhost:3001
# Override the model if you want:
#   OLLAMA_MODEL=qwen2.5:7b-instruct-q4_K_M cargo run

# 2. Frontend (new terminal)
cd frontend
cp .env.example .env     # then edit and paste your VITE_MAPBOX_TOKEN
npm install
npm run dev              # opens the first free port at 5173+
```

Visit the Vite URL. Within ~10 s of backend startup the map will populate;
within ~45 s the narrator panel will start scrolling.

## Endpoints

| Method | Path | Description |
|---|---|---|
| GET | `/api/health`       | `{ok: true}` |
| GET | `/api/aircraft`     | Latest cached `Snapshot` |
| GET | `/ws`               | Snapshot stream (one frame per fetcher tick) |
| GET | `/ws/narration`     | Narration stream (one frame per narrator tick) |

## License

MIT
