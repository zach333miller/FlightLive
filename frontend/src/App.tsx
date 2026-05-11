import { useEffect, useRef } from 'react'
import mapboxgl from 'mapbox-gl'
import 'mapbox-gl/dist/mapbox-gl.css'
import './App.css'

const MAPBOX_TOKEN = import.meta.env.VITE_MAPBOX_TOKEN as string

// Garyville, LA (Marathon refinery area)
const GARYVILLE: [number, number] = [-90.6173, 30.0735]

function App() {
  const mapContainer = useRef<HTMLDivElement | null>(null)
  const mapRef = useRef<mapboxgl.Map | null>(null)

  useEffect(() => {
    if (!mapContainer.current || mapRef.current) return

    if (!MAPBOX_TOKEN) {
      console.error('Missing VITE_MAPBOX_TOKEN in frontend/.env')
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

  return (
    <div style={{ position: 'fixed', inset: 0 }}>
      <div ref={mapContainer} style={{ width: '100%', height: '100%' }} />
      <div
        style={{
          position: 'absolute',
          top: 12,
          left: 12,
          padding: '8px 12px',
          background: 'rgba(0,0,0,0.6)',
          color: 'white',
          font: '14px system-ui',
          borderRadius: 6,
          pointerEvents: 'none',
        }}
      >
        FlightLive — Garyville LA
      </div>
    </div>
  )
}

export default App
