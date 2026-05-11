import { useEffect, useRef, useState } from 'react'
import mapboxgl from 'mapbox-gl'
import 'mapbox-gl/dist/mapbox-gl.css'
import './App.css'

const MAPBOX_TOKEN = import.meta.env.VITE_MAPBOX_TOKEN as string

// Garyville, LA (Marathon refinery area) — initial map center.
const GARYVILLE: [number, number] = [-90.6173, 30.0735]
const POLL_MS = 5000

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
}

function createMarkerEl(): HTMLDivElement {
  const el = document.createElement('div')
  el.style.width = '22px'
  el.style.height = '22px'
  el.style.display = 'flex'
  el.style.alignItems = 'center'
  el.style.justifyContent = 'center'
  el.style.cursor = 'pointer'
  el.innerHTML = `
    <svg viewBox="0 0 24 24" width="22" height="22" xmlns="http://www.w3.org/2000/svg">
      <path d="M12 2 L18 20 L12 16 L6 20 Z" fill="#fbbf24" stroke="#000" stroke-width="0.8"/>
    </svg>
  `
  return el
}

function popupHtml(a: Aircraft): string {
  const altFt = a.altitude_m != null ? Math.round(a.altitude_m * 3.281) : null
  const speedKt = a.velocity_ms != null ? Math.round(a.velocity_ms * 1.944) : null
  const heading = a.heading != null ? Math.round(a.heading) : null
  return `
    <div style="font: 13px system-ui; color: #111; min-width: 160px;">
      <div style="font-weight: 600; font-size: 14px;">
        ${a.callsign ?? a.icao24}
      </div>
      <div style="color: #555; margin-bottom: 6px;">${a.origin_country}</div>
      <div>${altFt != null ? altFt.toLocaleString() + ' ft' : 'on ground'}</div>
      ${speedKt != null ? `<div>${speedKt} kt</div>` : ''}
      ${heading != null ? `<div>heading ${heading}°</div>` : ''}
      <div style="color: #888; font-size: 11px; margin-top: 4px;">
        ICAO ${a.icao24}
      </div>
    </div>
  `
}

function App() {
  const mapContainer = useRef<HTMLDivElement | null>(null)
  const mapRef = useRef<mapboxgl.Map | null>(null)
  const markersRef = useRef<Map<string, mapboxgl.Marker>>(new Map())
  const [aircraft, setAircraft] = useState<Aircraft[]>([])
  const [lastUpdate, setLastUpdate] = useState<Date | null>(null)
  const [error, setError] = useState<string | null>(null)

  // Init Mapbox once on mount; tear down on unmount.
  useEffect(() => {
    if (!mapContainer.current || mapRef.current) return
    if (!MAPBOX_TOKEN) {
      setError('Missing VITE_MAPBOX_TOKEN in frontend/.env')
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

  // Poll the backend every POLL_MS for the latest aircraft.
  useEffect(() => {
    let cancelled = false

    async function fetchAircraft() {
      try {
        const res = await fetch('/api/aircraft')
        if (!res.ok) throw new Error(`HTTP ${res.status}`)
        const data: Aircraft[] = await res.json()
        if (cancelled) return
        setAircraft(data)
        setLastUpdate(new Date())
        setError(null)
      } catch (e) {
        if (!cancelled) setError(String(e))
      }
    }

    fetchAircraft()
    const id = setInterval(fetchAircraft, POLL_MS)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [])

  // Reconcile the marker layer with the latest aircraft list.
  // We keep a stable Map<icao24, Marker> in a ref so we mutate existing markers
  // in place (smooth) instead of recreating them every poll (jittery + slow).
  useEffect(() => {
    const map = mapRef.current
    if (!map) return

    const seen = new Set<string>()

    for (const a of aircraft) {
      seen.add(a.icao24)
      let m = markersRef.current.get(a.icao24)

      if (!m) {
        const el = createMarkerEl()
        m = new mapboxgl.Marker({ element: el, rotationAlignment: 'map' })
          .setLngLat([a.longitude, a.latitude])
          .setPopup(new mapboxgl.Popup({ offset: 14 }).setHTML(popupHtml(a)))
          .addTo(map)
        markersRef.current.set(a.icao24, m)
      } else {
        m.setLngLat([a.longitude, a.latitude])
        m.getPopup()?.setHTML(popupHtml(a))
      }

      m.setRotation(a.heading ?? 0)
    }

    // Drop markers for aircraft that left the bounding box.
    for (const [icao, m] of markersRef.current) {
      if (!seen.has(icao)) {
        m.remove()
        markersRef.current.delete(icao)
      }
    }
  }, [aircraft])

  return (
    <div style={{ position: 'fixed', inset: 0 }}>
      <div ref={mapContainer} style={{ width: '100%', height: '100%' }} />
      <div
        style={{
          position: 'absolute',
          top: 12,
          left: 12,
          padding: '10px 14px',
          background: 'rgba(0,0,0,0.75)',
          color: 'white',
          font: '13px system-ui',
          borderRadius: 8,
          pointerEvents: 'none',
          lineHeight: 1.5,
          minWidth: 200,
        }}
      >
        <div style={{ fontWeight: 600, fontSize: 14 }}>FlightLive — Garyville LA</div>
        <div>
          <span style={{ color: '#fbbf24', fontWeight: 600 }}>{aircraft.length}</span>{' '}
          aircraft in box
        </div>
        {lastUpdate && (
          <div style={{ opacity: 0.7, fontSize: 11 }}>
            updated {lastUpdate.toLocaleTimeString()}
          </div>
        )}
        {error && (
          <div style={{ color: '#f87171', fontSize: 11, marginTop: 4 }}>{error}</div>
        )}
      </div>
    </div>
  )
}

export default App
