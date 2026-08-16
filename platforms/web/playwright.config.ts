import { defineConfig, devices } from '@playwright/test'

// Playwright e2e for the OneAI webUI. The app under test is the Vite dev
// server; it connects its ws to a mock app-server (e2e/mock-server.ts) booted
// in globalSetup on :8788. Deterministic, Rust-free.
export default defineConfig({
  testDir: './e2e',
  fullyParallel: false, // the mock is a single shared server
  workers: 1,
  reporter: 'list',
  globalSetup: './e2e/globalSetup.ts',
  globalTeardown: './e2e/globalTeardown.ts',
  use: {
    baseURL: 'http://127.0.0.1:5173',
    trace: 'retain-on-failure',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: {
    command: 'npm run dev',
    url: 'http://127.0.0.1:5173',
    timeout: 60_000,
    reuseExistingServer: !process.env.CI,
    env: {
      // Point the SPA at the mock ws server, not the default real app-server.
      VITE_APP_SERVER_URL: 'ws://127.0.0.1:8788',
    },
  },
})
