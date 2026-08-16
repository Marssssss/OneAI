// Playwright global setup — boots the mock ws app-server before tests and
// tears it down after. The vite dev server (the app under test) is started by
// Playwright's own `webServer` config with VITE_APP_SERVER_URL pointing here.
import { startMockServer } from './mock-server'

export default async function globalSetup(): Promise<void> {
  const server = await startMockServer()
  // Stash the closer on globalThis so globalTeardown can reach it.
  ;(globalThis as unknown as { __e2eMockClose?: () => Promise<void> }).__e2eMockClose =
    server.close
}
