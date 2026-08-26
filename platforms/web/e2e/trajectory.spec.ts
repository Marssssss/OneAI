import { expect, test } from '@playwright/test'

// Issue #40 — the resident trajectory view: the header-right toggle switches
// the center column between chat and a horizontal swim-lane timeline; nodes
// are color-coded and clickable into a type-specific detail pane.

const composerOf = (page: import('@playwright/test').Page) =>
  page.getByPlaceholder(/想什么|thinking/i)

const toggleBtn = (page: import('@playwright/test').Page) =>
  page.getByRole('button', { name: /轨迹|Trajectory/ })

test('toggles to the trajectory view and shows the execution timeline', async ({ page }) => {
  await page.goto('/')
  const composer = composerOf(page)
  await expect(composer).toBeVisible()

  // Drive a trajectory-rich turn through the mock.
  await composer.pressSequentially('run the trajectory')
  await composer.press('Enter')

  // Wait for the turn to settle, then switch to the trajectory view.
  await expect(page.getByText('run the trajectory')).toBeVisible()
  await toggleBtn(page).click()

  // The swim-lane canvas renders (the timeline svg) with the legend present.
  const canvas = page.getByTestId('trajectory-timeline')
  await expect(canvas).toBeVisible()
  await expect(page.getByText(/图例|Legend/)).toBeVisible()
  // A delegation lane label + a tool node's title are present.
  await expect(page.getByText(/main/).first()).toBeVisible()

  // Switch back to the conversation (exact match — the sidebar "新对话" also
  // contains the substring "对话").
  await page.getByRole('button', { name: '对话', exact: true }).click()
  await expect(page.getByText('run the trajectory')).toBeVisible()
})

test('clicking a tool node reveals args + result in the detail pane', async ({ page }) => {
  await page.goto('/')
  const composer = composerOf(page)
  await composer.pressSequentially('run the trajectory')
  await composer.press('Enter')
  await expect(page.getByText('run the trajectory')).toBeVisible()

  await toggleBtn(page).click()
  const canvas = page.getByTestId('trajectory-timeline')
  await expect(canvas).toBeVisible()

  // Click the tool marker (the shape inside the tool_calls node group — a
  // circle for instants, a rect for a timed tool span).
  await canvas.locator('g[data-kind="tool_calls"] > circle, g[data-kind="tool_calls"] > rect').first().click()
  // The detail pane shows the tool command + result.
  await expect(page.getByText(/工具指令|Tool command/)).toBeVisible()
  await expect(page.getByText('file1')).toBeVisible()
})

test('clicking an infer node reveals the API request/response detail', async ({ page }) => {
  await page.goto('/')
  const composer = composerOf(page)
  await composer.pressSequentially('run the trajectory')
  await composer.press('Enter')
  await expect(page.getByText('run the trajectory')).toBeVisible()

  await toggleBtn(page).click()
  const canvas = page.getByTestId('trajectory-timeline')
  await expect(canvas).toBeVisible()

  // Click the infer node (the iteration_start marker).
  await canvas.locator('g[data-kind="iteration_start"] > circle').first().click()
  // The detail pane shows the API request/response drill-in (model + raw
  // request/response messages from the inference snapshot).
  await expect(page.getByText(/API 请求|API request/).first()).toBeVisible()
  await expect(page.getByText('gpt-4o')).toBeVisible()
})
