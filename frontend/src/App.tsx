import { useEffect, useRef, useState } from 'react'
import mapboxgl, { type GeoJSONSource } from 'mapbox-gl'
import 'mapbox-gl/dist/mapbox-gl.css'
import './App.css'

const MAPBOX_TOKEN = import.meta.env.VITE_MAPBOX_TOKEN as string
const GARYVILLE: [number, number] = [-90.6173, 30.0735]
const LISTENER_LNG = -90.628
const LISTENER_LAT = 30.063
const NM_PER_DEG_LAT = 60
const M_PER_DEG_LAT = 111_111

type Behavior =
  | 'CRUISE'
  | 'APPROACH'
  | 'HOLDING'
  | 'HOVERING'
  | 'CLIMBING'
  | 'DESCENDING'
  | 'TAXIING'
  | 'ENROUTE'

interface Aircraft {
  icao24: string
  callsign: string | null
  origin_country: string
  longitude: number
  latitude: number
  altitude_m: number | null
  velocity_ms: number | null
  heading: number | null
  on_ground: boolean
  behavior: Behavior
  trail: [number, number][]
}

interface Conflict {
  a_icao: string
  b_icao: string
  a_callsign: string | null
  b_callsign: string | null
  horizontal_nm: number
  vertical_ft: number
  seconds_from_now: number
  at_lng: number
  at_lat: number
}

interface AudibleEvent {
  icao24: string
  callsign: string | null
  closest_approach_in_s: number
  closest_distance_nm: number
  closest_slant_m: number
  estimated_db: number
}

interface Snapshot {
  time: number
  fetched_at_ms: number
  aircraft: Aircraft[]
  conflicts: Conflict[]
  audible: AudibleEvent[]
  listener: [number, number]
}

interface Narration {
  at_ms: number
  text: string
  aircraft_count: number
}

interface Track {
  aircraft: Aircraft
  anchor_lng: number
  anchor_lat: number
  anchor_ms: number
}

function altitudeColor(a: Aircraft): string {
  if (a.on_ground) return '#6b7280'
  const alt = a.altitude_m ?? 0
  if (alt < 300) return '#ef4444'
  if (alt < 1500) return '#f97316'
  if (alt < 3000) return '#fbbf24'
  if (alt < 6000) return '#84cc16'
  if (alt < 10000) return '#22c55e'
  return '#06b6d4'
}

function behaviorBadge(b: Behavior): { color: string; label: string } {
  switch (b) {
    case 'HOVERING': return { color: '#a855f7', label: 'HOVERING' }
    case 'HOLDING': return { color: '#ec4899', label: 'HOLDING' }
    case 'APPROACH': return { color: '#f59e0b', label: 'APPROACH' }
    case 'CLIMBING': return { color: '#3b82f6', label: 'CLIMBING' }
    case 'DESCENDING': return { color: '#0ea5e9', label: 'DESCENDING' }
    case 'CRUISE': return { color: '#10b981', label: 'CRUISE' }
    case 'TAXIING': return { color: '#737373', label: 'TAXIING' }
    case 'ENROUTE': default: return { color: '#525252', label: 'EN ROUTE' }
  }
}

function createMarkerEl(color: string): HTMLDivElement {
  const el = document.createElement('div')
  el.style.width = '22px'
  el.style.height = '22px'
  el.style.cursor = 'pointer'
  el.innerHTML = `
    <svg viewBox="0 0 24 24" width="22" height="22"
         style="filter: drop-shadow(0 0 2px rgba(0,0,0,0.6));">
      <path d="M12 2 L18 20 L12 16 L6 20 Z" fill="${color}" stroke="#000" stroke-width="0.8"/>
    </svg>
  `
  return el
}

function updateMarkerColor(el: HTMLElement, color: string) {
  const path = el.querySelector('path')
  if (path) path.setAttribute('fill', color)
}

function deadReckon(
  lng: number,
  lat: number,
  heading_deg: number,
  speed_ms: number,
  dt_s: number,
): [number, number] {
  const h = (heading_deg * Math.PI) / 180
  const m_north = speed_ms * Math.cos(h) * dt_s
  const m_east = speed_ms * Math.sin(h) * dt_s
  const d_lat = m_north / M_PER_DEG_LAT
  const d_lng = m_east / (M_PER_DEG_LAT * Math.cos((lat * Math.PI) / 180))
  return [lng + d_lng, lat + d_lat]
}

// Approximate circle as polygon for the 5 NM drone-ops ring around the refinery.
function circlePolygon(
  lng: number,
  lat: number,
  radius_nm: number,
  steps = 64,
): [number, number][] {
  const coords: [number, number][] = []
  const lat_rad = (lat * Math.PI) / 180
  for (let i = 0; i <= steps; i++) {
    const theta = (i / steps) * Math.PI * 2
    const d_lat = (radius_nm / NM_PER_DEG_LAT) * Math.cos(theta)
    const d_lng =
      (radius_nm / NM_PER_DEG_LAT) * Math.sin(theta) / Math.cos(lat_rad)
    coords.push([lng + d_lng, lat + d_lat])
  }
  return coords
}

// Hand-approximated Marathon Garyville fence-line polygon (refinery footprint).
const REFINERY_POLYGON: [number, number][] = [
  [-90.652, 30.054],
  [-90.652, 30.077],
  [-90.612, 30.077],
  [-90.612, 30.054],
  [-90.652, 30.054],
]

const DRONE_RING_5NM: [number, number][] = circlePolygon(LISTENER_LNG, LISTENER_LAT, 5)

function App() {
  const mapContainer = useRef<HTMLDivElement | null>(null)
  const mapRef = useRef<mapboxgl.Map | null>(null)
  const mapReadyRef = useRef(false)
  const tracksRef = useRef<Map<string, Track>>(new Map())
  const markersRef = useRef<Map<string, mapboxgl.Marker>>(new Map())
  const wsSnapRef = useRef<WebSocket | null>(null)
  const wsNarrRef = useRef<WebSocket | null>(null)
  const rafRef = useRef<number | null>(null)
  const selectedIcaoRef = useRef<string | null>(null)

  const [aircraft, setAircraft] = useState<Aircraft[]>([])
  const [conflicts, setConflicts] = useState<Conflict[]>([])
  const [audible, setAudible] = useState<AudibleEvent[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [connected, setConnected] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [lastSnapshotMs, setLastSnapshotMs] = useState<number | null>(null)
  const [narrations, setNarrations] = useState<Narration[]>([])
  const [weatherTime, setWeatherTime] = useState<number | null>(null)

  selectedIcaoRef.current = selected

  // ---- map init + sources/layers ----
  useEffect(() => {
    if (!mapContainer.current || mapRef.current) return
    if (!MAPBOX_TOKEN) {
      setError('Missing VITE_MAPBOX_TOKEN')
      return
    }

    mapboxgl.accessToken = MAPBOX_TOKEN
    const map = new mapboxgl.Map({
      container: mapContainer.current,
      style: 'mapbox://styles/mapbox/dark-v11',
      center: GARYVILLE,
      zoom: 9,
    })
    map.addControl(new mapboxgl.NavigationControl(), 'top-right')
    mapRef.current = map

    map.on('load', () => {
      // ---- weather radar (RainViewer) — set up empty, populate in another effect ----
      map.addSource('weather', {
        type: 'raster',
        tiles: [],
        tileSize: 256,
      })
      map.addLayer(
        {
          id: 'weather-layer',
          type: 'raster',
          source: 'weather',
          paint: { 'raster-opacity': 0.55 },
        },
        // Insert below labels so city names stay readable.
        firstLabelLayerId(map),
      )

      // ---- refinery polygon ----
      map.addSource('refinery', {
        type: 'geojson',
        data: {
          type: 'Feature',
          geometry: { type: 'Polygon', coordinates: [REFINERY_POLYGON] },
          properties: {},
        },
      })
      map.addLayer({
        id: 'refinery-fill',
        type: 'fill',
        source: 'refinery',
        paint: { 'fill-color': '#fbbf24', 'fill-opacity': 0.12 },
      })
      map.addLayer({
        id: 'refinery-line',
        type: 'line',
        source: 'refinery',
        paint: { 'line-color': '#fbbf24', 'line-width': 1.5, 'line-dasharray': [2, 1] },
      })

      // ---- 5 NM drone-ops ring ----
      map.addSource('drone-ring', {
        type: 'geojson',
        data: {
          type: 'Feature',
          geometry: { type: 'Polygon', coordinates: [DRONE_RING_5NM] },
          properties: {},
        },
      })
      map.addLayer({
        id: 'drone-ring-line',
        type: 'line',
        source: 'drone-ring',
        paint: { 'line-color': '#a855f7', 'line-width': 1, 'line-dasharray': [3, 2], 'line-opacity': 0.6 },
      })

      // ---- listener marker (refinery) ----
      const listenerEl = document.createElement('div')
      listenerEl.innerHTML = `
        <svg width="22" height="22" viewBox="0 0 24 24">
          <circle cx="12" cy="12" r="6" fill="#fbbf24" stroke="#000" stroke-width="1"/>
          <circle cx="12" cy="12" r="2" fill="#000"/>
        </svg>
      `
      new mapboxgl.Marker({ element: listenerEl })
        .setLngLat([LISTENER_LNG, LISTENER_LAT])
        .setPopup(new mapboxgl.Popup().setText('Marathon Garyville — acoustic listener'))
        .addTo(map)

      // ---- flight trails (empty initially) ----
      map.addSource('trails', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      })
      map.addLayer({
        id: 'trails-line',
        type: 'line',
        source: 'trails',
        paint: {
          'line-color': ['get', 'color'],
          'line-width': 1.5,
          'line-opacity': 0.55,
        },
      })

      // ---- conflict pairs ----
      map.addSource('conflicts', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      })
      map.addLayer({
        id: 'conflicts-line',
        type: 'line',
        source: 'conflicts',
        paint: { 'line-color': '#ef4444', 'line-width': 2.5, 'line-opacity': 0.85, 'line-dasharray': [1, 1] },
      })
      map.addLayer({
        id: 'conflicts-circle',
        type: 'circle',
        source: 'conflicts',
        filter: ['==', ['geometry-type'], 'Point'],
        paint: {
          'circle-radius': 14,
          'circle-color': 'transparent',
          'circle-stroke-color': '#ef4444',
          'circle-stroke-width': 2,
          'circle-opacity': 1,
        },
      })

      mapReadyRef.current = true
    })

    return () => {
      map.remove()
      mapRef.current = null
      mapReadyRef.current = false
    }
  }, [])

  // ---- RainViewer: poll every 5 min, set weather source tiles ----
  useEffect(() => {
    let cancelled = false
    async function refresh() {
      try {
        const r = await fetch('https://api.rainviewer.com/public/weather-maps.json')
        const j = await r.json()
        const past = j.radar?.past as { path: string; time: number }[] | undefined
        if (!past || past.length === 0) return
        const latest = past[past.length - 1]
        if (cancelled) return
        const tiles = [`https://tilecache.rainviewer.com${latest.path}/256/{z}/{x}/{y}/2/1_1.png`]
        const map = mapRef.current
        if (map && map.isStyleLoaded() && map.getSource('weather')) {
          ;(map.getSource('weather') as mapboxgl.RasterSource).tiles = tiles
          map.style.sourceCaches['weather']?.clearTiles()
          map.style.sourceCaches['weather']?.update(map.transform)
          map.triggerRepaint()
        } else {
          // Map not ready yet; retry when load fires.
          mapRef.current?.once('load', () => refresh())
        }
        setWeatherTime(latest.time * 1000)
      } catch (err) {
        console.warn('rainviewer fetch failed', err)
      }
    }
    refresh()
    const id = setInterval(refresh, 5 * 60 * 1000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [])

  // ---- WebSocket: snapshots ----
  useEffect(() => {
    let stopped = false
    let retry: ReturnType<typeof setTimeout> | null = null

    function connect() {
      if (stopped) return
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
      const ws = new WebSocket(`${proto}//${location.host}/ws`)
      wsSnapRef.current = ws

      ws.onopen = () => {
        setConnected(true)
        setError(null)
      }
      ws.onmessage = (e) => {
        try {
          const snap: Snapshot = JSON.parse(e.data)
          const now = performance.now()
          for (const a of snap.aircraft) {
            tracksRef.current.set(a.icao24, {
              aircraft: a,
              anchor_lng: a.longitude,
              anchor_lat: a.latitude,
              anchor_ms: now,
            })
          }
          const present = new Set(snap.aircraft.map((a) => a.icao24))
          for (const icao of [...tracksRef.current.keys()]) {
            if (!present.has(icao)) tracksRef.current.delete(icao)
          }
          setAircraft(snap.aircraft)
          setConflicts(snap.conflicts)
          setAudible(snap.audible)
          setLastSnapshotMs(Date.now())
        } catch (err) {
          console.error('bad snapshot', err)
        }
      }
      ws.onerror = () => setError('WebSocket error')
      ws.onclose = () => {
        setConnected(false)
        if (!stopped) retry = setTimeout(connect, 2000)
      }
    }

    connect()
    return () => {
      stopped = true
      if (retry) clearTimeout(retry)
      wsSnapRef.current?.close()
    }
  }, [])

  // ---- WebSocket: narrations ----
  useEffect(() => {
    let stopped = false
    let retry: ReturnType<typeof setTimeout> | null = null

    function connect() {
      if (stopped) return
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
      const ws = new WebSocket(`${proto}//${location.host}/ws/narration`)
      wsNarrRef.current = ws
      ws.onmessage = (e) => {
        try {
          const n: Narration = JSON.parse(e.data)
          setNarrations((prev) => [n, ...prev].slice(0, 8))
        } catch {}
      }
      ws.onclose = () => {
        if (!stopped) retry = setTimeout(connect, 2000)
      }
    }
    connect()
    return () => {
      stopped = true
      if (retry) clearTimeout(retry)
      wsNarrRef.current?.close()
    }
  }, [])

  // ---- Marker reconcile on snapshot ----
  useEffect(() => {
    const map = mapRef.current
    if (!map || !mapReadyRef.current) return

    const seen = new Set<string>()
    for (const a of aircraft) {
      seen.add(a.icao24)
      const color = altitudeColor(a)
      let m = markersRef.current.get(a.icao24)
      if (!m) {
        const el = createMarkerEl(color)
        el.addEventListener('click', (ev) => {
          ev.stopPropagation()
          setSelected(a.icao24)
        })
        m = new mapboxgl.Marker({ element: el, rotationAlignment: 'map' })
          .setLngLat([a.longitude, a.latitude])
          .addTo(map)
        markersRef.current.set(a.icao24, m)
      } else {
        updateMarkerColor(m.getElement(), color)
      }
      m.setRotation(a.heading ?? 0)
    }
    for (const [icao, m] of markersRef.current) {
      if (!seen.has(icao)) {
        m.remove()
        markersRef.current.delete(icao)
      }
    }
  }, [aircraft])

  // ---- Update trails GeoJSON whenever the snapshot changes ----
  useEffect(() => {
    const map = mapRef.current
    if (!map || !mapReadyRef.current) return
    const src = map.getSource('trails') as GeoJSONSource | undefined
    if (!src) return
    const features = aircraft
      .filter((a) => a.trail.length >= 2)
      .map((a) => ({
        type: 'Feature' as const,
        geometry: { type: 'LineString' as const, coordinates: a.trail },
        properties: { color: altitudeColor(a), icao: a.icao24 },
      }))
    src.setData({ type: 'FeatureCollection', features })
  }, [aircraft])

  // ---- Update conflicts overlay ----
  useEffect(() => {
    const map = mapRef.current
    if (!map || !mapReadyRef.current) return
    const src = map.getSource('conflicts') as GeoJSONSource | undefined
    if (!src) return

    type AnyFeature =
      | {
          type: 'Feature'
          geometry: { type: 'LineString'; coordinates: [number, number][] }
          properties: Record<string, unknown>
        }
      | {
          type: 'Feature'
          geometry: { type: 'Point'; coordinates: [number, number] }
          properties: Record<string, unknown>
        }

    const features: AnyFeature[] = []
    for (const c of conflicts) {
      const a = aircraft.find((x) => x.icao24 === c.a_icao)
      const b = aircraft.find((x) => x.icao24 === c.b_icao)
      if (!a || !b) continue
      features.push({
        type: 'Feature',
        geometry: {
          type: 'LineString',
          coordinates: [
            [a.longitude, a.latitude],
            [b.longitude, b.latitude],
          ],
        },
        properties: { seconds_from_now: c.seconds_from_now },
      })
      features.push({
        type: 'Feature',
        geometry: { type: 'Point', coordinates: [c.at_lng, c.at_lat] },
        properties: {},
      })
    }
    src.setData({ type: 'FeatureCollection', features } as unknown as GeoJSON.FeatureCollection)
  }, [conflicts, aircraft])

  // ---- requestAnimationFrame: dead-reckon marker positions ----
  useEffect(() => {
    function step() {
      const map = mapRef.current
      if (map) {
        const now = performance.now()
        for (const [icao, t] of tracksRef.current) {
          const m = markersRef.current.get(icao)
          if (!m) continue
          const v = t.aircraft.velocity_ms ?? 0
          const h = t.aircraft.heading ?? 0
          if (v < 1 || t.aircraft.on_ground) {
            m.setLngLat([t.anchor_lng, t.anchor_lat])
          } else {
            const dt = (now - t.anchor_ms) / 1000
            const [lng, lat] = deadReckon(t.anchor_lng, t.anchor_lat, h, v, dt)
            m.setLngLat([lng, lat])
          }
        }
      }
      rafRef.current = requestAnimationFrame(step)
    }
    rafRef.current = requestAnimationFrame(step)
    return () => {
      if (rafRef.current != null) cancelAnimationFrame(rafRef.current)
    }
  }, [])

  const selectedAc = selected ? aircraft.find((a) => a.icao24 === selected) ?? null : null

  return (
    <div style={{ position: 'fixed', inset: 0 }}>
      <div ref={mapContainer} style={{ width: '100%', height: '100%' }} />

      {/* Status badge (top-left) */}
      <div
        style={{
          position: 'absolute',
          top: 12,
          left: 12,
          padding: '10px 14px',
          background: 'rgba(0,0,0,0.78)',
          color: 'white',
          font: '13px system-ui',
          borderRadius: 8,
          lineHeight: 1.5,
          minWidth: 260,
          pointerEvents: 'none',
        }}
      >
        <div style={{ fontWeight: 700, fontSize: 14, marginBottom: 2 }}>
          FlightLive — Garyville LA
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <span style={{ color: '#fbbf24', fontWeight: 600 }}>{aircraft.length}</span>
          <span>aircraft</span>
          {conflicts.length > 0 && (
            <span style={{ color: '#ef4444', fontWeight: 600, marginLeft: 6 }}>
              ⚠ {conflicts.length} conflict{conflicts.length === 1 ? '' : 's'}
            </span>
          )}
          <span
            style={{
              padding: '2px 7px',
              borderRadius: 4,
              fontSize: 11,
              fontWeight: 600,
              background: connected ? '#16a34a' : '#dc2626',
              marginLeft: 'auto',
            }}
          >
            {connected ? 'LIVE' : 'OFFLINE'}
          </span>
        </div>
        {lastSnapshotMs && (
          <div style={{ opacity: 0.7, fontSize: 11 }}>
            snapshot {new Date(lastSnapshotMs).toLocaleTimeString()}
            {weatherTime && (
              <>
                {' · '}weather {new Date(weatherTime).toLocaleTimeString()}
              </>
            )}
          </div>
        )}
        {error && <div style={{ color: '#f87171', fontSize: 11, marginTop: 4 }}>{error}</div>}
      </div>

      {/* Altitude legend (bottom-left) */}
      <div
        style={{
          position: 'absolute',
          bottom: 28,
          left: 12,
          padding: '10px 14px',
          background: 'rgba(0,0,0,0.78)',
          color: 'white',
          font: '12px system-ui',
          borderRadius: 8,
          lineHeight: 1.7,
          pointerEvents: 'none',
        }}
      >
        <div style={{ fontWeight: 600, marginBottom: 4 }}>Altitude</div>
        {(
          [
            ['#ef4444', '< 300 m'],
            ['#f97316', '< 1.5 km'],
            ['#fbbf24', '< 3 km'],
            ['#84cc16', '< 6 km'],
            ['#22c55e', '< 10 km'],
            ['#06b6d4', '≥ 10 km'],
            ['#6b7280', 'on ground'],
          ] as const
        ).map(([color, label]) => (
          <div key={label} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <span
              style={{
                display: 'inline-block',
                width: 10,
                height: 10,
                borderRadius: 2,
                background: color,
                border: '1px solid rgba(255,255,255,0.2)',
              }}
            />
            {label}
          </div>
        ))}
      </div>

      {/* Acoustic ticker (bottom-center) */}
      <div
        style={{
          position: 'absolute',
          bottom: 28,
          left: '50%',
          transform: 'translateX(-50%)',
          padding: '10px 14px',
          background: 'rgba(0,0,0,0.82)',
          color: 'white',
          font: '12px system-ui',
          borderRadius: 8,
          lineHeight: 1.5,
          maxWidth: 520,
          pointerEvents: 'none',
        }}
      >
        <div style={{ fontWeight: 600, marginBottom: 4 }}>
          🔊 Audible at refinery — next 4 min
        </div>
        {audible.length === 0 ? (
          <div style={{ opacity: 0.65 }}>none predicted</div>
        ) : (
          audible.slice(0, 3).map((ev) => (
            <div
              key={ev.icao24}
              style={{ display: 'flex', gap: 12, fontVariantNumeric: 'tabular-nums' }}
            >
              <span style={{ minWidth: 64, fontWeight: 600 }}>
                {ev.callsign ?? ev.icao24}
              </span>
              <span style={{ minWidth: 56 }}>
                in {Math.round(ev.closest_approach_in_s)}s
              </span>
              <span style={{ minWidth: 64 }}>
                {ev.closest_distance_nm.toFixed(1)} NM
              </span>
              <span style={{ color: '#fbbf24' }}>{Math.round(ev.estimated_db)} dB</span>
            </div>
          ))
        )}
      </div>

      {/* Narrator news ticker (right side) */}
      <div
        style={{
          position: 'absolute',
          right: 12,
          bottom: 28,
          width: 320,
          maxHeight: '55vh',
          padding: '12px 14px',
          background: 'rgba(0,0,0,0.82)',
          color: 'white',
          font: '13px system-ui',
          borderRadius: 10,
          lineHeight: 1.5,
          overflowY: 'auto',
        }}
      >
        <div style={{ fontWeight: 700, marginBottom: 6, display: 'flex', alignItems: 'center', gap: 6 }}>
          🤖 Airspace narrator
          <span style={{ fontSize: 10, opacity: 0.6, fontWeight: 400 }}>
            llama3.1:8b · local
          </span>
        </div>
        {narrations.length === 0 ? (
          <div style={{ opacity: 0.6, fontStyle: 'italic' }}>warming up…</div>
        ) : (
          narrations.map((n, i) => (
            <div
              key={n.at_ms}
              style={{
                marginBottom: 10,
                paddingBottom: 10,
                borderBottom: i === narrations.length - 1 ? 'none' : '1px solid rgba(255,255,255,0.1)',
                opacity: i === 0 ? 1 : 0.75 - i * 0.1,
              }}
            >
              <div style={{ fontSize: 10, opacity: 0.5, marginBottom: 3 }}>
                {new Date(n.at_ms).toLocaleTimeString()} · {n.aircraft_count} aircraft
              </div>
              <div>{n.text}</div>
            </div>
          ))
        )}
      </div>

      {/* Side panel (top-right) */}
      {selectedAc && (
        <div
          style={{
            position: 'absolute',
            top: 12,
            right: 12,
            width: 320,
            padding: '16px 18px',
            background: 'rgba(0,0,0,0.92)',
            color: 'white',
            font: '13px system-ui',
            borderRadius: 10,
            lineHeight: 1.7,
            boxShadow: '0 4px 14px rgba(0,0,0,0.45)',
          }}
        >
          <div
            style={{
              display: 'flex',
              justifyContent: 'space-between',
              alignItems: 'flex-start',
            }}
          >
            <div style={{ fontWeight: 700, fontSize: 18, lineHeight: 1.2 }}>
              {selectedAc.callsign ?? selectedAc.icao24}
            </div>
            <button
              onClick={() => setSelected(null)}
              style={{
                background: 'none',
                border: 'none',
                color: 'white',
                fontSize: 20,
                cursor: 'pointer',
                padding: 0,
                marginLeft: 12,
                lineHeight: 1,
              }}
              aria-label="Close"
            >
              ×
            </button>
          </div>
          <div style={{ color: '#9ca3af', fontSize: 12, marginBottom: 8 }}>
            {selectedAc.origin_country}
          </div>
          <div style={{ marginBottom: 12 }}>
            <span
              style={{
                display: 'inline-block',
                padding: '3px 8px',
                background: behaviorBadge(selectedAc.behavior).color,
                color: 'white',
                fontSize: 11,
                fontWeight: 600,
                borderRadius: 4,
                letterSpacing: 0.5,
              }}
            >
              {behaviorBadge(selectedAc.behavior).label}
            </span>
          </div>
          <Detail
            label="Altitude"
            value={
              selectedAc.on_ground
                ? 'on ground'
                : selectedAc.altitude_m != null
                  ? `${Math.round(selectedAc.altitude_m * 3.281).toLocaleString()} ft  (${Math.round(selectedAc.altitude_m).toLocaleString()} m)`
                  : 'unknown'
            }
          />
          <Detail
            label="Ground speed"
            value={
              selectedAc.velocity_ms != null
                ? `${Math.round(selectedAc.velocity_ms * 1.944)} kt  (${Math.round(selectedAc.velocity_ms)} m/s)`
                : 'unknown'
            }
          />
          <Detail
            label="Heading"
            value={selectedAc.heading != null ? `${Math.round(selectedAc.heading)}°` : 'unknown'}
          />
          <Detail
            label="Position"
            value={`${selectedAc.latitude.toFixed(4)}, ${selectedAc.longitude.toFixed(4)}`}
          />
          <Detail label="Trail points" value={`${selectedAc.trail.length}`} />
          <Detail label="ICAO24" value={selectedAc.icao24} mono />
        </div>
      )}
    </div>
  )
}

function Detail({
  label,
  value,
  mono = false,
}: {
  label: string
  value: string
  mono?: boolean
}) {
  return (
    <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12 }}>
      <span style={{ color: '#9ca3af' }}>{label}</span>
      <span style={{ fontFamily: mono ? 'ui-monospace, Consolas, monospace' : undefined }}>
        {value}
      </span>
    </div>
  )
}

// Find the first label layer so we can insert the raster weather layer beneath it.
function firstLabelLayerId(map: mapboxgl.Map): string | undefined {
  const layers = map.getStyle().layers
  if (!layers) return undefined
  for (const l of layers) {
    if (l.type === 'symbol' && (l.layout as { 'text-field'?: unknown })?.['text-field']) {
      return l.id
    }
  }
  return undefined
}

export default App
