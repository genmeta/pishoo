import { expect, test } from '@playwright/test'

test.beforeEach(async ({ page }) => {
  await page.route('**/std/workspace-api/context', (route) => route.fulfill({ json: {
    profile: 'alice.dhttp.net', owner_name: 'alice.dhttp.net',
    badges: { pending_reviews: 0, incoming_contacts: 0 },
  } }))
  await page.route('**/std/workspace-api/settings/profile', (route) => route.fulfill({ json: {
    identity_name: 'alice.dhttp.net', display_name: null, avatar_url: null, updated_at: 0,
  } }))
})

test('apps show WASM metadata and declared API paths across locales', async ({ page }, testInfo) => {
  await page.route('**/std/workspace-api/libs', (route) => route.fulfill({ json: [{
    id: 'demo', title: 'Demo component', version: '1.2.3', description: 'Echo incoming data',
    endpoints: [{ method: 'POST', path: '/std/api/demo/echo', description: 'Echo data' }],
  }] }))
  await page.goto('./apps')
  await expect(page.getByRole('heading', { name: 'Demo component' })).toBeVisible()
  await expect(page.getByText('WASM', { exact: true })).toBeVisible()
  await expect(page.getByText('Version 1.2.3')).toBeVisible()
  await expect(page.getByText('Echo incoming data')).toBeVisible()
  await expect(page.locator('.capability-card-header a')).toHaveCount(0)
  await page.getByText('API endpoints (1)').click()
  await expect(page.getByText('/std/api/demo/echo', { exact: true })).toBeVisible()
  await expect(page.getByText('Echo data', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Chinese' }).click()
  await expect(page.getByRole('heading', { name: '应用管理' })).toBeVisible()
  await expect(page.getByText('API 接口（1）')).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('wasm-apps.png'), fullPage: true })
  const widths = await page.evaluate(() => ({
    viewport: document.documentElement.clientWidth, content: document.documentElement.scrollWidth,
  }))
  expect(widths.content).toBeLessThanOrEqual(widths.viewport)
  await page.reload()
  await expect(page.getByRole('heading', { name: 'Demo component' })).toBeVisible()
})

test('GET API paths open in a new tab and all operations show descriptions', async ({ page, context }) => {
  await page.route('**/std/workspace-api/libs', (route) => route.fulfill({ json: [{
    id: 'note', title: 'note', version: '0.1.0', description: 'Private notes',
    endpoints: [
      { method: 'GET', path: '/std/api/note/', description: 'Note page' },
      { method: 'GET', path: '/std/api/note/notes', description: 'Notes list' },
      { method: 'POST', path: '/std/api/note/notes', description: 'Created' },
    ],
  }] }))
  await context.route('**/std/api/note/', (route) => route.fulfill({
    contentType: 'text/html', body: '<!doctype html><html><body><h1>Note page</h1></body></html>',
  }))
  await page.goto('./apps')
  await expect(page.locator('.capability-card-header a')).toHaveCount(0)
  await page.locator('.capability-card').click({ position: { x: 10, y: 10 } })
  expect(context.pages()).toHaveLength(1)
  await page.getByText('API endpoints (3)').click()
  const link = page.getByRole('link', { name: 'Open GET /std/api/note/ in a new tab', exact: true })
  await expect(link).toHaveAttribute('href', '/std/api/note/')
  await expect(page.getByText('Note page', { exact: true })).toBeVisible()
  await expect(page.getByText('Notes list', { exact: true })).toBeVisible()
  await expect(page.getByText('Created', { exact: true })).toBeVisible()
  await expect(page.locator('.capability-endpoints a')).toHaveCount(2)
  await expect(page.locator('.capability-endpoints li').filter({ hasText: 'POST' }).getByRole('link')).toHaveCount(0)
  expect(context.pages()).toHaveLength(1)

  const popupPromise = page.waitForEvent('popup')
  await link.click()
  const popup = await popupPromise
  await expect(popup).toHaveURL(/\/std\/api\/note\/$/)
  await expect(popup.getByRole('heading', { name: 'Note page' })).toBeVisible()
  await expect(page).toHaveURL(/\/std\/workspace\/apps$/)
  await popup.close()

  await link.focus()
  const keyboardPopupPromise = page.waitForEvent('popup')
  await page.keyboard.press('Enter')
  const keyboardPopup = await keyboardPopupPromise
  await expect(keyboardPopup.getByRole('heading', { name: 'Note page' })).toBeVisible()
  await keyboardPopup.close()
})

test('apps distinguish empty lists and failed requests and support retry', async ({ page }) => {
  let fail = true
  await page.route('**/std/workspace-api/libs', (route) => fail
    ? route.fulfill({ status: 503, body: 'Components unavailable' })
    : route.fulfill({ json: [] }))
  await page.goto('./apps')
  await expect(page.getByRole('alert')).toContainText('Components unavailable')
  await expect(page.getByText('No apps loaded')).toHaveCount(0)
  fail = false
  await page.getByRole('button', { name: 'Retry' }).click()
  await expect(page.getByText('No apps loaded')).toBeVisible()
  await expect(page.getByRole('alert')).toHaveCount(0)
})
