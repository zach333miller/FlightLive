import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      // HTTP API
      '/api': 'http://localhost:3001',
      // WebSocket — `ws: true` tells Vite to handle the HTTP upgrade
      '/ws': {
        target: 'ws://localhost:3001',
        ws: true,
      },
    },
  },
})
