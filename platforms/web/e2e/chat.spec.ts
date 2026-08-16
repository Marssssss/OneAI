import { expect, test } from '@playwright/test'

// Loads the SPA (connected to the mock ws app-server), sends a message, and
// asserts the streamed assistant reply renders.

test('streamed reply appears after send', async ({ page }) => {
  await page.goto('/')

  // The composer textarea placeholder (zh "告诉我你在想什么…", en "Tell me what
  // you're thinking…") — match either locale.
  const composer = page.getByPlaceholder(/想什么|thinking/i)
  await expect(composer).toBeVisible()

  // Type + Enter to send (Enter sends; Shift+Enter is newline).
  await composer.fill('hi')
  await composer.press('Enter')

  // The mock streams "Hello from the OneAI mock server!" — assert it lands.
  await expect(
    page.getByText(/Hello from the OneAI mock server/).first(),
  ).toBeVisible({ timeout: 10_000 })
})
