# FlightLive

> A pre-flight check tool for drone operators flying near Marathon
> Garyville refinery (Louisiana). One page, three live data sources,
> derived intelligence on top — everything a Part 107 pilot needs in
> the 30 seconds before pressing launch.

![FlightLive — live aircraft, weather, and radar over Garyville LA](docs/screenshot.png)

[![Open in GitHub Codespaces](https://github.com/codespaces/badge.svg)](https://codespaces.new/zach333miller/FlightLive?quickstart=1)

> ⚠️ **Portfolio demo, not an operational tool.** Free public ADS-B
> aggregators run 5–15 s behind real time — too stale for a
> flight-safety decision. For real Part 107 ops, cross-reference
> [B4UFLY](https://www.faa.gov/uas/getting_started/b4ufly) and a paid
> commercial feed.

## Why it exists

The standard pre-flight workflow at the refinery is to juggle four
tabs: B4UFLY for airspace, an aviation weather site for METARs, NOAA
for radar, and ADS-B Exchange or FlightAware for nearby traffic.
This consolidates all of it into a single view locked to the
refinery's GPS coordinates.

It's also a deliberate first-Rust project — going from zero Rust
experience to a working full-stack `Rust + tokio + axum + React + WS`
app in one session.

## What's on screen

| Layer | Source |
|---|---|
| Live aircraft positions, heading-aligned arrows, altitude-band color | OpenSky Network API (OAuth2) |
| Per-aircraft trail (~80 s history) | Server-side ring buffer |
| Behavior label (CRUISE / APPROACH / HOLDING / HOVERING / CLIMBING / DESCENDING / TAXIING / ENROUTE) | Rust classifier over trajectory history |
| Pairwise conflict detection (3 NM / 1000 ft, t=0 & t=60 s dead-reckoned) | Rust spatial routine |
| Acoustic prediction — "which aircraft will be audible at the refinery in the next 4 min" | Slant-distance attenuation + per-class source-noise model |
| METAR weather card (flight category, wind, visibility, ceiling, temp/dew, altimeter, raw) | aviationweather.gov — KAPS (Reserve LA) |
| Animated NEXRAD precipitation radar — last 60 min in 5-min frames | Iowa State Environmental Mesonet |
| Refinery fence-line polygon + 5 NM drone-ops boundary | Hand-defined GeoJSON |

The map view is locked to the OpenSky bounding box around the
refinery — no zoom, no pan, no UI to fiddle with. Open, glance,
decide.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ External feeds                                               │
│  · OpenSky /api/states/all  (OAuth2, 10 s)                   │
│  · aviationweather.gov /api/data/metar  (5 min)              │
│  · IEM NEXRAD tile cache  (front-end fetch)                  │
└────────────────────┬─────────────────────────────────────────┘
                     │
┌────────────────────▼─────────────────────────────────────────┐
│ Rust backend (port 3001)                                     │
│  fetcher_task (10 s):                                        │
│    writes Arc<RwLock<HistoryMap>>                            │
│    classifies Behavior over history                          │
│    detects conflicts (t=0 & t=60 s)                          │
│    predicts audible-at-refinery events                       │
│    joins latest weather snapshot                             │
│    writes cache + broadcasts → WS clients                    │
│  weather_task (5 min): KAPS METAR → Arc<RwLock<Option<…>>>   │
│  /api/aircraft  → cache · /ws → snapshot stream              │
└────────────────────┬─────────────────────────────────────────┘
                     │  WS frames
┌────────────────────▼─────────────────────────────────────────┐
│ React + Mapbox (Vite)                                        │
│  · light-v11 basemap, locked bounding box                    │
│  · WS subscriber → state                                     │
│  · requestAnimationFrame dead-reckons marker positions       │
│    + trail polylines between OpenSky ticks                   │
│  · IEM radar tiles cycled at 1.4 fps                         │
└──────────────────────────────────────────────────────────────┘
```

## Engineering details

- `Arc<RwLock<…>>` shared state, many readers + occasional writer
- `tokio::sync::broadcast` fan-out, one producer → N WS sessions
- `tokio::select!` multiplexing broadcast receive with disconnect
- OAuth2 client-credentials token exchange + cached bearer (refreshes 30 s before expiry)
- Tolerant decode of OpenSky's heterogeneous positional arrays via `serde_json::Value` + `filter_map`
- Pure-function spatial math (haversine, dead-reckoning) — unit-testable
- Bounded ring buffers (`VecDeque<TrackPoint>`) per aircraft for history
- Frontend dead-reckons marker positions between 10 s OpenSky updates so motion is smooth; trail polylines extend their last vertex to the reckoned point so lines never detach from the plane
- Map locked (`interactive: false`) so the operator can't accidentally pan away mid-check

## Run locally

```bash
# Backend (anonymous OpenSky tier by default — ~100 req/day cap)
cd backend && cargo run
# Or with credentials for the 4 000 req/day tier:
#   OPENSKY_CLIENT_ID=… OPENSKY_CLIENT_SECRET=… cargo run

# Frontend
cd frontend
cp .env.example .env       # paste your Mapbox public token
npm install && npm run dev # Vite opens at the first free port from 5173
```

Map populates ~10 s after backend startup (first OpenSky fetch + first METAR).

## Standalone executable

The release build embeds the React bundle in the Rust binary via
[`rust-embed`](https://crates.io/crates/rust-embed) — one ~6 MB `.exe`
boots the API, serves the SPA on the same origin, and opens the
browser. No Node, no Vite, no separate frontend server in prod.

```bash
cd frontend && npm run build        # Vite inlines VITE_MAPBOX_TOKEN
cd ../backend && cargo build --release
# → backend/target/release/flightlive.exe
```

## OpenSky setup (for the 4 000 req/day tier)

OpenSky moved from HTTP Basic Auth to OAuth2 in 2024–25.

1. Create a free account at <https://opensky-network.org/>.
2. Account → API Client → **Reset Credential**.
3. Copy `clientId` + `clientSecret` (secret shown once).
4. Set as env vars before running:
   `OPENSKY_CLIENT_ID=… OPENSKY_CLIENT_SECRET=…`.

`backend/src/opensky.rs` handles the token exchange against the
Keycloak endpoint at `auth.opensky-network.org` and caches the bearer
token until 30 s before expiry.

## Stack

Rust 1.95 · Axum 0.7 (ws) · `tokio` (full) · `serde` · `reqwest` ·
`tower-http` · `rust-embed` · React 18 · TypeScript · Vite · Mapbox
GL JS · OpenSky · aviationweather.gov · Iowa State IEM.

## License

MIT
