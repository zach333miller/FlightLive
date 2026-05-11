import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    // Forward any /api/* request from the Vite dev server (:5173+) to the
    // Rust backend on :3001. The browser thinks it's same-origin, so no
    // CORS preflight and no hardcoded URLs in the React code.
    proxy: {
      '/api': 'http://localhost:3001',
    },
  },
})
