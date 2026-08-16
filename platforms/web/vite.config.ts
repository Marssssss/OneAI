/// <reference types="vitest" />
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// OneAI webUI — Vite dev/preview + Vitest.
//
// The webUI talks to `oneai app-server --listen ws://127.0.0.1:8787` over raw
// WebSocket (no HTTP path routing — the app-server binds a TCP+WS listener).
// In dev, Vite serves the SPA on :5173 and the browser opens the ws directly
// to the app-server's address (configurable via VITE_APP_SERVER_URL, default
// ws://127.0.0.1:8787). No Vite proxy is needed because ws is a separate host.
export default defineConfig({
  plugins: [react()],
  server: {
    host: '127.0.0.1', // bind IPv4 explicitly — vite's default `localhost` may
    // resolve to IPv6 ::1 only on some hosts, breaking IPv4 probes (e2e).
    port: 5173,
    strictPort: false,
  },
  build: {
    outDir: 'dist',
    sourcemap: true,
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
})
