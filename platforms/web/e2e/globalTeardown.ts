// Playwright global teardown — close the mock ws server started in globalSetup.
export default async function globalTeardown(): Promise<void> {
  const close = (globalThis as unknown as { __e2eMockClose?: () => Promise<void> })
    .__e2eMockClose
  if (close) await close()
}
