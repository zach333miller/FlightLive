import { useEffect, useRef, useState } from 'react'
import mapboxgl from 'mapbox-gl'
import 'mapbox-gl/dist/mapbox-gl.css'
import './App.css'

const MAPBOX_TOKEN = import.meta.env.VITE_MAPBOX_TOKEN as string
const GARYVILLE: [number, number] = [-90.6173, 30.0735]
const M_PER_DEG_LAT = 111_111

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
  time_position: number | null
}

interface Snapshot {
  time: number
  fetched_at_ms: number
  aircraft: Aircraft[]
}

// Per-aircraft tracking state for dead-reckoning between snapshots.
interface Track {
  aircraft: Aircraft
  anchor_lng: number
  anchor_lat: number
  anchor_ms: number // performance.now() when the snapshot arrived
}

function altitudeColor(a: Aircraft): string {
  if (a.on_ground) return '#6b7280'
  const alt = a.altitude_m ?? 0
  if (alt < 300) return '#ef4444'   // red — refinery-conflict altitude
  if (alt < 1500) return '#f97316'  // orange
  if (alt < 3000) return '#fbbf24'  // yellow
  if (alt < 6000) return '#84cc16'  // lime
  if (alt < 10000) return '#22c55e' // green
  return '#06b6d4'                   // cyan — FL330+ cruise
}

function createMarkerEl(color: string): HTMLDivElement {
  const el = document.createElement('div')
  el.style.width = '22px'
  el.style.height = '22px'
  el.style.cursor = 'pointer'
  el.innerHTML = `
    <svg viewBox="0 0 24 24" width="22" height="22" xmlns="http://www.w3.org/2000/svg"
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

// Project a (lng, lat) forward by dt seconds at the given heading and speed.
// Heading 0° = north (lat+), 90° = east (lng+).
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

function App() {
  const mapContainer = useRef<HTMLDivElement | null>(null)
  const mapRef = useRef<mapboxgl.Map | null>(null)
  const tracksRef = useRef<Map<string, Track>>(new Map())
  const markersRef = useRef<Map<string, mapboxgl.Marker>>(new Map())
  const wsRef = useRef<WebSocket | null>(null)
  const rafRef = useRef<number | null>(null)

  const [aircraft, setAircraft] = useState<Aircraft[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [connected, setConnected] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [lastSnapshotMs, setLastSnapshotMs] = useState<number | null>(null)

  // ---- map init ----
  useEffect(() => {
    if (!mapContainer.current || mapRef.current) return
    if (!MAPBOX_TOKEN) {
      setError('Missing VITE_MAPBOX_TOKEN')
      return
    }

    mapboxgl.accessToken = MAPBOX_TOKEN
    mapRef.current = new mapboxgl.Map({
      container: mapContainer.current,
      style: 'mapbox://styles/mapbox/dark-v11',
      center: GARYVILLE,
      zoom: 9,
    })
    mapRef.current.addControl(new mapboxgl.NavigationControl(), 'top-right')

    return () => {
      mapRef.current?.remove()
      mapRef.current = null
    }
  }, [])

  // ---- WebSocket: connect, parse, refresh anchor positions, auto-reconnect ----
  useEffect(() => {
    let stopped = false
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null

    function connect() {
      if (stopped) return
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
      const ws = new WebSocket(`${proto}//${location.host}/ws`)
      wsRef.current = ws

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
          setLastSnapshotMs(Date.now())
        } catch (err) {
          console.error('bad snapshot', err)
        }
      }
      ws.onerror = () => setError('WebSocket error')
      ws.onclose = () => {
        setConnected(false)
        if (!stopped) reconnectTimer = setTimeout(connect, 2000)
      }
    }

    connect()
    return () => {
      stopped = true
      if (reconnectTimer) clearTimeout(reconnectTimer)
      wsRef.current?.close()
    }
  }, [])

  // ---- marker reconciliation on each snapshot ----
  useEffect(() => {
    const map = mapRef.current
    if (!map) return

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

  // ---- render loop: dead-reckon every animation frame ----
  // requestAnimationFrame fires ~60Hz; for ~30 markers this is essentially free.
  // We extrapolate forward from the most recent anchor using each aircraft's
  // velocity + heading, giving the visual a continuous glide rather than
  // discrete jumps every 10 seconds when OpenSky refreshes.
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
          minWidth: 240,
          pointerEvents: 'none',
        }}
      >
        <div style={{ fontWeight: 600, fontSize: 14, marginBottom: 2 }}>
          FlightLive — Garyville LA
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <span style={{ color: '#fbbf24', fontWeight: 600 }}>{aircraft.length}</span>
          <span>aircraft</span>
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
            ['#ef4444', '< 300 m  (refinery alt)'],
            ['#f97316', '< 1.5 km'],
            ['#fbbf24', '< 3 km'],
            ['#84cc16', '< 6 km'],
            ['#22c55e', '< 10 km'],
            ['#06b6d4', '≥ 10 km  (cruise)'],
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

      {/* Side panel (top-right) */}
      {selectedAc && (
        <div
          style={{
            position: 'absolute',
            top: 12,
            right: 12,
            width: 290,
            padding: '16px 18px',
            background: 'rgba(0,0,0,0.88)',
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
          <div style={{ color: '#9ca3af', fontSize: 12, marginBottom: 12 }}>
            {selectedAc.origin_country}
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

export default App
