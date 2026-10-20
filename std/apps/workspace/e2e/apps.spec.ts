import { expect, test } from '@playwright/test'
import type { LibSummary, StoreDetail } from '../src/api/types'

const publisher = 'alice.dhttp.net'
const app: LibSummary = { id: `${publisher}/calendar`, kind: 'app', title: 'Calendar', version: '1.0.0', description: 'Calendar pages', dependencies: [], endpoints: [], entry: '/' }
const bytes = Buffer.from('signed-package-is-validated-by-the-install-endpoint')
const release: StoreDetail = { ...app, publisher, name: 'calendar', publisher_owner_hash: 'a'.repeat(64), status: 'published', status_reason: '', published_at: 1, package_sha256: 'b'.repeat(64), size: 1024, download_path: `/store/v1/apps/${app.id}/versions/1.0.0/app.webc`, signer_certificates: [], description: app.description!, changelog: 'Initial release', host: null }

test.beforeEach(async ({ page }) => {
  page.on('request', request => {
    const url = new URL(request.url())
    expect(url.pathname).not.toMatch(/^\/std\/sys\/(lib|app)\/(loaded|check|openapi)(\/|$)/)
    if (/^\/std\/sys\/(lib|app)$/.test(url.pathname)) {
      expect(request.method()).not.toBe('POST')
      if (request.method() === 'PUT') {
        expect(url.searchParams.size).toBe(1)
        expect(url.searchParams.has('name')).toBe(true)
        expect(request.headers()['content-type']).toBe('application/json')
        expect(Object.keys(request.postDataJSON()).sort()).toEqual(['publisher', 'version'])
      }
    }
  })
  await page.route('**/std/profile', route => route.fulfill({ json: { identity_name: publisher, display_name: 'Alice', avatar_url: null, updated_at: 0 } }))
  await page.route('**/std/acl/reviews?*', route => route.fulfill({ json: { items: [], total: 0, page: 1, page_size: 1 } }))
  await page.route('**/std/acl/promissions', route => route.fulfill({ json: {} }))
  await page.route('**/std/acl/allow', route => route.fulfill({ json: {} }))
  await page.route('**/std/acl/access', route => route.fulfill({ json: {} }))
  await page.route('**/std/contacts?*', route => route.fulfill({ json: { items: [], total: 0, page: 1, page_size: 100 } }))
  await page.route('**/std/sys/app/list', route => route.fulfill({ json: [] }))
  await page.route('**/std/sys/lib/list', route => route.fulfill({ json: [] }))
})

test('installed cards and details use only the database catalog', async ({ page }) => {
  await page.route('**/std/sys/app/list', route => route.fulfill({ json: [app] }))
  await page.goto('/workspace/apps')
  await expect(page.locator('.app-card')).toContainText('v1.0.0')
  await expect(page.getByRole('link', { name: 'Open ↗', exact: true })).toHaveAttribute('href', '/calendar/')
  await page.getByRole('link', { name: 'Details', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Calendar', exact: true })).toBeVisible()
  await expect(page.getByText('Running version', { exact: true })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Signature & host capabilities', exact: true })).toHaveCount(0)
})

test('installed App and Lib pages have no local package installation entry', async ({ page }) => {
  for (const kind of ['app', 'lib']) {
    await page.goto(`/workspace/apps?kind=${kind}`)
    await expect(page.getByRole('heading', { name: 'Apps', exact: true })).toBeVisible()
    await expect(page.getByRole('button', { name: /Install from file|Update from file|Inspect component/ })).toHaveCount(0)
    await expect(page.locator('input[type="file"]')).toHaveCount(0)
  }
})

test('store installation uses JSON PUT and preserves the exact release for retry', async ({ page }) => {
  let attempts = 0, installed: LibSummary[] = []
  await page.route('**/std/sys/app/list', route => route.fulfill({ json: installed }))
  await page.route(`**/store/v1/apps/${app.id}/versions/1.0.0`, route => route.fulfill({ json: release }))
  await page.route('**/std/sys/app?name=calendar', route => {
    expect(route.request().postDataJSON()).toEqual({ publisher, version: '1.0.0' })
    if (++attempts === 1) return route.fulfill({ status: 400, body: 'Publisher signature verification failed' })
    installed = [app]
    return route.fulfill({ json: app })
  })
  await page.goto(`/workspace/apps/store/${app.id}?version=1.0.0`)
  await page.getByRole('button', { name: 'Install v1.0.0', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog.locator('input[type="file"]')).toHaveCount(0)
  await dialog.getByRole('checkbox').check()
  await dialog.getByRole('button', { name: 'Confirm installation', exact: true }).click()
  await expect(dialog.getByRole('alert')).toContainText('Publisher signature verification failed')
  await expect(dialog.getByRole('checkbox')).toBeChecked()
  await dialog.getByRole('button', { name: 'Confirm installation', exact: true }).click()
  await expect(dialog).not.toBeVisible()
  await page.goto('/workspace/apps')
  await expect(page.locator('.app-card')).toContainText('Calendar')
  expect(attempts).toBe(2)
})

test('Lib installation uses the selected store release and no local upload', async ({ page }) => {
  const lib: LibSummary = { ...app, kind: 'lib', id: `${publisher}/message`, entry: undefined }
  const libRelease: StoreDetail = { ...release, ...lib, kind: 'lib', name: 'message', host: {},
    openapi: { openapi: '3.1.0', info: { title: 'Message', version: '1.0.0' }, paths: {} } }
  await page.route(`**/store/v1/libs/${lib.id}/versions/1.0.0`, route => route.fulfill({ json: libRelease }))
  let installed = false
  await page.route('**/std/sys/lib?name=message', route => {
    expect(route.request().postDataJSON()).toEqual({ publisher, version: '1.0.0' })
    installed = true
    return route.fulfill({ json: lib })
  })
  await page.goto(`/workspace/apps/store/${lib.id}?kind=lib&version=1.0.0`)
  await page.getByRole('button', { name: 'Install v1.0.0', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog.locator('input[type="file"]')).toHaveCount(0)
  await dialog.getByRole('checkbox').check()
  await dialog.getByRole('button', { name: 'Confirm installation', exact: true }).click()
  await expect(dialog).not.toBeVisible()
  expect(installed).toBe(true)
})

test('installed Update resolves a store version before sending the same JSON PUT', async ({ page }) => {
  const next = { ...app, version: '2.0.0' }
  const nextRelease: StoreDetail = { ...release, version: '2.0.0', changelog: 'New store release',
    download_path: `/store/v1/apps/${app.id}/versions/2.0.0/app.webc` }
  let installed = [app], updated = false
  await page.route('**/std/sys/app/list', route => route.fulfill({ json: installed }))
  await page.route(`**/store/v1/apps/${app.id}`, route => route.fulfill({ json: nextRelease }))
  await page.route('**/std/sys/app?name=calendar', route => {
    expect(route.request().postDataJSON()).toEqual({ publisher, version: '2.0.0' })
    updated = true; installed = [next]
    return route.fulfill({ json: next })
  })
  await page.goto('/workspace/apps')
  await page.getByRole('button', { name: 'Update', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toContainText('Calendar · v2.0.0')
  await expect(dialog).toContainText('New store release')
  await expect(dialog.locator('input[type="file"]')).toHaveCount(0)
  await dialog.getByRole('checkbox').check()
  await dialog.getByRole('button', { name: 'Confirm update', exact: true }).click()
  await expect(dialog).not.toBeVisible()
  await expect(page.locator('.app-card')).toContainText('v2.0.0')
  expect(updated).toBe(true)
})

test('switching package type discards a delayed update preview before any installation', async ({ page }) => {
  const lib: LibSummary = { ...app, kind: 'lib', title: 'Calendar API', entry: undefined }
  const libRelease: StoreDetail = { ...release, ...lib, kind: 'lib', version: '2.0.0', host: {},
    openapi: { openapi: '3.1.0', info: { title: 'Calendar API', version: '2.0.0' }, paths: {} } }
  const next = { ...app, version: '2.0.0' }
  const appRelease: StoreDetail = { ...release, version: '2.0.0' }
  let installed = [app], requested = false
  let deliver!: () => void
  const pending = new Promise<void>(resolve => { deliver = resolve })
  const hostWrites: string[] = []
  await page.route('**/std/sys/app/list', route => route.fulfill({ json: installed }))
  await page.route('**/std/sys/lib/list', route => route.fulfill({ json: [lib] }))
  await page.route(`**/store/v1/libs/${lib.id}`, async route => {
    requested = true
    await pending
    return route.fulfill({ json: libRelease })
  })
  await page.route(`**/store/v1/apps/${app.id}`, route => route.fulfill({ json: appRelease }))
  await page.route('**/std/sys/app?name=calendar', route => {
    hostWrites.push(route.request().url())
    expect(route.request().postDataJSON()).toEqual({ publisher, version: '2.0.0' })
    installed = [next]
    return route.fulfill({ json: next })
  })
  await page.route('**/std/sys/lib?name=calendar', route => {
    hostWrites.push(route.request().url())
    return route.fulfill({ status: 500, body: 'Unexpected stale Lib installation' })
  })
  await page.goto('/workspace/apps?kind=lib')
  await page.getByRole('button', { name: 'Update', exact: true }).click()
  await expect.poll(() => requested).toBe(true)
  await page.getByRole('combobox', { name: 'Install type', exact: true }).selectOption('app')
  await expect(page.locator('.app-card')).toContainText('Calendar pages')
  deliver()
  await expect(page.getByRole('button', { name: 'Update', exact: true })).toBeEnabled()
  await expect(page.getByRole('dialog')).not.toBeVisible()
  expect(hostWrites).toEqual([])
  await page.getByRole('button', { name: 'Update', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toContainText('Calendar · v2.0.0')
  await dialog.getByRole('checkbox').check()
  await dialog.getByRole('button', { name: 'Confirm update', exact: true }).click()
  await expect(dialog).not.toBeVisible()
  await expect(page.locator('.app-card')).toContainText('v2.0.0')
  expect(hostWrites).toHaveLength(1)
  expect(new URL(hostWrites[0]).pathname).toBe('/std/sys/app')
})

test('uninstall pins the complete registered identity and removes its catalog card', async ({ page }) => {
  let installed = [app]
  await page.route('**/std/sys/app/list', route => route.fulfill({ json: installed }))
  await page.route('**/std/sys/app?name=calendar', route => {
    expect(route.request().method()).toBe('DELETE')
    expect(route.request().headers()['expected-package-id']).toBe(app.id)
    installed = []
    return route.fulfill({ status: 204 })
  })
  await page.goto('/workspace/apps')
  await page.getByRole('button', { name: 'Uninstall', exact: true }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Confirm uninstall', exact: true }).click()
  await expect(page.locator('.app-card')).toHaveCount(0)
  await expect(page.getByRole('status')).toContainText('Uninstalled')
})

test('publishing uploads original signed bytes at an explicit author and version', async ({ page }) => {
  let published = false
  await page.route('**/store/v1/apps?*', route => route.fulfill({ json: { apps: [], page: 1, page_size: 20, total: 0 } }))
  await page.route(`**/store/v1/apps/${app.id}/versions/1.0.0`, route => {
    if (route.request().method() === 'PUT') { expect(route.request().postDataBuffer()).toEqual(bytes); published = true }
    return route.fulfill({ json: release })
  })
  await page.goto('/workspace/apps?tab=store')
  await page.getByRole('button', { name: 'Upload app', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await dialog.getByLabel('Application ID').fill('alice/calendar')
  await dialog.getByLabel('Version', { exact: true }).fill('1.0.0')
  await dialog.getByLabel('WEBC package file').setInputFiles({ name: 'app.webc', mimeType: 'application/webc', buffer: bytes })
  await dialog.getByRole('checkbox').check()
  await dialog.getByRole('button', { name: 'Publish version', exact: true }).click()
  await expect(dialog).not.toBeVisible()
  expect(published).toBe(true)
  await expect(page).toHaveURL(/\/workspace\/apps\/store\/alice.dhttp.net\/calendar\?.*version=1.0.0/)
})

test('update checks batch complete database registrations at their installed versions', async ({ page }) => {
  const items = Array.from({ length: 102 }, (_, index) => ({ ...app, id: `${publisher}/calendar-${index}` }))
  const batches: number[] = []
  await page.route('**/std/sys/app/list', route => route.fulfill({ json: [...items, { ...app, id: 'legacy' }] }))
  await page.route('**/store/v1/updates', route => {
    const input = route.request().postDataJSON().apps as Array<{ publisher: string; name: string; version: string }>
    batches.push(input.length)
    expect(input.every(item => item.publisher === publisher && item.version === '1.0.0')).toBe(true)
    return route.fulfill({ json: { apps: input.map(item => ({ id: `${item.publisher}/${item.name}`, status: 'current' })) } })
  })
  await page.goto('/workspace/apps')
  await page.getByRole('button', { name: 'Check for updates', exact: true }).click()
  await expect(page.getByRole('status')).toContainText('No updates available')
  expect(batches).toEqual([100, 2])
})
