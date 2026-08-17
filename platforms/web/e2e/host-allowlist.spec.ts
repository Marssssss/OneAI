import { expect, test } from '@playwright/test'

// §B5 — the durable host allow/deny list renders in the Settings "Network"
// section against the mock app-server's `host/list`, and removing a row fires
// `host/remove` + optimistically drops it. Deterministic — no engine.

test('host allowlist lists + removes a host (B5)', async ({ page }) => {
  await page.goto('/')

  // Open the Settings modal via the sidebar footer button (aria-label = 设置,
  // the default zh locale).
  await page.getByRole('button', { name: '设置' }).click()

  // The Settings modal has a left rail; click the Network section.
  const rail = page.locator('nav button', { hasText: '网络授权' })
  await rail.click()

  // The mock seeds one admitted host: api.example.com.
  const hostRow = page.locator('div', { hasText: 'api.example.com' }).first()
  await expect(hostRow).toBeVisible()

  // The row's delete button fires `host/remove`; the row optimistically
  // disappears and the empty-state message replaces it.
  const delBtn = hostRow.getByRole('button', { name: '删除' })
  await delBtn.click()

  // The host is gone — the panel shows the empty-state for the allowed list.
  await expect(page.locator('text=暂无记录').first()).toBeVisible()
  await expect(hostRow).toHaveCount(0)
})
