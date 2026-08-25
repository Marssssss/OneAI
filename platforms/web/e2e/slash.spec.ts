import { expect, test } from '@playwright/test'

// Slash-command palette (issue #39): TUI-aligned scope, functional grouping,
// two-level subcommand hints. The commands exercised here are all
// client-side or covered by the mock server (/help renders a note;
// /session list hits the mock's empty session/list) — no engine needed.

const composerOf = (page: import('@playwright/test').Page) =>
  page.getByPlaceholder(/想什么|thinking/i)

test('palette lists the TUI-aligned set, grouped, without scenario/settings entries', async ({
  page,
}) => {
  await page.goto('/')
  const composer = composerOf(page)
  await expect(composer).toBeVisible()
  await composer.pressSequentially('/')

  const options = page.getByRole('option')
  for (const label of [
    '/new',
    '/clear',
    '/compact',
    '/session',
    '/usage',
    '/help',
    '/init',
    '/skills',
    '/domain',
  ]) {
    await expect(options.filter({ hasText: label })).toBeVisible()
  }
  // The removed web-only commands must not surface.
  await expect(options.filter({ hasText: '/scenario' })).toHaveCount(0)
  await expect(options.filter({ hasText: '/settings' })).toHaveCount(0)
  await expect(options.filter({ hasText: '/plan' })).toHaveCount(0)
  await expect(options.filter({ hasText: '/trajectory' })).toHaveCount(0)

  // Group headers render (zh default locale: 会话 / 查看 / 项目 / 扩展).
  await expect(page.getByText(/会话|Session/).first()).toBeVisible()
  await expect(page.getByText(/扩展|Extensions/).first()).toBeVisible()
})

test('second-level subcommands surface after `<cmd> `', async ({ page }) => {
  await page.goto('/')
  const composer = composerOf(page)
  await expect(composer).toBeVisible()
  await composer.pressSequentially('/session ')

  const options = page.getByRole('option')
  await expect(options.filter({ hasText: '/session list' })).toBeVisible()
  await expect(options.filter({ hasText: '/session resume' })).toBeVisible()
  // The top-level entry itself is gone from the second-level list.
  await expect(options).toHaveCount(2)

  // Free-form zone: past the subcommand the popup closes.
  await composer.pressSequentially('resume abc')
  await expect(options).toHaveCount(0)
})

test('/help prints the command sheet as an in-chat note', async ({ page }) => {
  await page.goto('/')
  const composer = composerOf(page)
  await expect(composer).toBeVisible()
  await composer.pressSequentially('/help')
  await composer.press('Enter')
  await expect(page.getByText(/可用命令|Available commands/)).toBeVisible()
})

test('/session list reports the (empty) mock session list', async ({ page }) => {
  await page.goto('/')
  const composer = composerOf(page)
  await expect(composer).toBeVisible()
  await composer.pressSequentially('/session list')
  await composer.press('Enter')
  await expect(page.getByText(/暂无已保存的会话|No saved sessions/)).toBeVisible()
})

test('unknown slash command shows a note and is never sent to the model', async ({
  page,
}) => {
  await page.goto('/')
  const composer = composerOf(page)
  await expect(composer).toBeVisible()
  await composer.pressSequentially('/definitely-not-a-command')
  await composer.press('Enter')

  await expect(page.getByText(/未知命令|Unknown command/)).toBeVisible()
  // Nothing was sent: the mock streams a canned reply for every turn/run, so
  // its absence proves the raw text never reached `turn/run`.
  await expect(page.getByText(/Hello from the OneAI mock server/)).toHaveCount(0)
})
