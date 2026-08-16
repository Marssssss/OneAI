import { expect, test } from '@playwright/test'

// A turn whose message contains "approve" makes the mock push a tool call
// that pauses for approval. Responding resumes the turn (tool_result + a
// short summary + turn_complete).

test('tool approval panel appears and resumes on allow', async ({ page }) => {
  await page.goto('/')

  const composer = page.getByPlaceholder(/OneAI/i)
  await expect(composer).toBeVisible()

  await composer.fill('please approve this')
  await composer.press('Enter')

  // The approval panel renders an "allow" button (zh "允许" / en "Allow").
  const allow = page.getByRole('button', { name: /^允许$|^Allow$/ })
  await expect(allow).toBeVisible({ timeout: 10_000 })

  // The pending tool node shows the shell tool call.
  await expect(page.getByText(/shell/).first()).toBeVisible()

  // Approve → the mock resumes with a "Done." summary.
  await allow.click()
  await expect(page.getByText(/Done\./).first()).toBeVisible({ timeout: 10_000 })
})
