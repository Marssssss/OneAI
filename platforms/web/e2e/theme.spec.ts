import { expect, test } from '@playwright/test'

// Toggling the theme flips the body's data-oneai-theme attribute, exercised
// on the desktop layout (sidebar footer toggle).

test('theme toggle flips the body attribute', async ({ page }) => {
  await page.goto('/')
  await expect(page).toHaveURL('/')

  const before = await page.evaluate(() =>
    document.body.getAttribute('data-oneai-theme'),
  )

  // The sidebar footer theme toggle — title is "切换主题" (zh) / "Toggle theme" (en).
  const toggle = page.getByTitle(/切换主题|Toggle theme/)
  await toggle.click()

  await expect(async () => {
    const after = await page.evaluate(() =>
      document.body.getAttribute('data-oneai-theme'),
    )
    expect(after).not.toBe(before)
  }).toPass()
})
