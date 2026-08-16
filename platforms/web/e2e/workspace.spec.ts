import { expect, test } from '@playwright/test'

// The workspace chip (left of the mode chip) opens a popover (not a modal) on
// the welcome/empty page; mid-conversation it shows a "start a new chat"
// prompt. Exercises the open path + the known-workspaces list + the native
// "add workspace" button (not clicked — the native OS picker can't run under
// Playwright against the mock server, and the mock doesn't implement
// `dialog/pick_directory`).

test('workspace chip opens a dropdown on the welcome page', async ({ page }) => {
  await page.goto('/')

  // The chip's aria-label is the localized "选择工作区".
  const chip = page.getByRole('button', { name: '选择工作区' })
  await expect(chip).toBeVisible()
  await chip.click()

  // Dropdown (popover) appears with the known-list subhead + the 添加工作区
  // button (opens the native OS folder picker via the sidecar).
  await expect(page.getByText('已知工作区')).toBeVisible()
  await expect(page.getByRole('button', { name: '添加工作区' })).toBeVisible()

  // Click-away closes it.
  await page.mouse.click(10, 10)
  await expect(page.getByText('已知工作区')).toBeHidden()
})
