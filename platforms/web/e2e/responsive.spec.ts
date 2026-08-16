import { expect, test } from '@playwright/test'

// Below the 900px breakpoint AppFrame switches to a single-column layout:
// a top bar with a hamburger, the sidebar as a slide-in drawer.

test('narrow viewport shows the hamburger and opens the drawer', async ({
  page,
}) => {
  // Set a phone-sized viewport BEFORE load so AppFrame's width state is mobile.
  await page.setViewportSize({ width: 375, height: 812 })
  await page.goto('/')

  // The hamburger nav button (aria-label "Open navigation") appears on mobile.
  const hamburger = page.getByRole('button', { name: 'Open navigation' })
  await expect(hamburger).toBeVisible()

  // On desktop the sidebar session list is in a column; on mobile it is hidden
  // until the drawer opens. Open the drawer.
  await hamburger.click()

  // The drawer's sidebar is now visible (scrim covers the rest).
  await expect(page.locator('[class*="drawerOpen"]')).toBeVisible()
})
