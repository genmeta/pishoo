import { expect, test } from '@playwright/test'
import { readFileSync } from 'node:fs'
import type { LibSummary, RulesByApi, StoreDetail, StoreSummary } from '../src/api/types'

const journal: LibSummary = {
  id: 'journal', title: 'Personal journal', version: '1.1.0', description: 'Keep your stories, photos and conversations together. Share reading and commenting with the people you choose.',
  endpoints: [
    { method: 'GET', path: '/api/journal/page', description: 'Open journal' },
    { method: 'GET', path: '/api/journal/entries', description: 'Read entries' },
    { method: 'POST', path: '/api/journal/comments', description: 'Write comments' },
  ],
  openapi: {
    openapi: '3.1.0', info: { title: 'Personal journal', version: '1.1.0', license: { name: 'MIT' } },
    'x-pishoo': {
      publisher: 'alice.smith.dhttp.net', name: 'journal', entry: '/page', changelog: 'A place for notes and memories. Existing journal data is preserved.',
      host: { 'private-storage': 'Save journal entries in this component’s private /db.', 'request-identity': 'Identify the owner and visiting readers.' },
      'access-groups': { read: { title: 'Reading', operations: ['page', 'entries'] }, comment: { title: 'Commenting', operations: ['comment'] } },
    },
    paths: {
      '/page': { get: { operationId: 'page', 'x-pishoo': { 'access-group': 'read' } } },
      '/entries': { get: { operationId: 'entries', 'x-pishoo': { 'access-group': 'read' } } },
      '/comments': { post: { operationId: 'comment', 'x-pishoo': { 'access-group': 'comment' } } },
    },
  },
}
const bob = 'bob.really-long-identity-name.dhttp.net'
const storeApi = '**/std/dhttp/store.dhttp.net/store/v1'
const fullId = 'alice.smith.dhttp.net/journal'

function completeJournal(version = '1.1.0'): LibSummary {
  return { ...journal, id: fullId, version,
    endpoints: journal.endpoints.map(op => ({ ...op, path: op.path.replace('/api/journal', `/api/${fullId}`) })),
    openapi: { ...journal.openapi!, info: { ...journal.openapi!.info, version } },
  }
}
function release(version = '1.2.0', status: 'published' | 'yanked' = 'published'): StoreDetail {
  return { id: fullId, publisher: 'alice.smith.dhttp.net', name: 'journal', version, status,
    publisher_owner_hash: 'a'.repeat(64), status_reason: status === 'yanked' ? 'Withdrawn by publisher' : '',
    package_sha256: 'b'.repeat(64), size: 4096, published_at: 1791850000,
    download_path: `/store/v1/libs/${fullId}/versions/${version}/lib.webc`,
    kind: 'lib', dependencies: [], title: journal.title, description: journal.description,
    changelog: 'A place for notes and memories. Existing journal data is preserved.',
    host: journal.openapi!['x-pishoo']!.host!, openapi: completeJournal(version).openapi!, signer_certificates: ['test-certificate'],
  }
}
function summary(version = '1.2.0', status: 'published' | 'yanked' = 'published'): StoreSummary {
  const { openapi, signer_certificates: _certificates, ...metadata } = release(version, status)
  return { ...metadata, title: openapi!.info.title, description: journal.description,
    changelog: openapi!['x-pishoo']!.changelog!, host: openapi!['x-pishoo']!.host! }
}

test.beforeEach(async ({ page }) => {
  // All requests are intercepted; these tests never use a profile database.
  await page.route('**/std/workspace-api/context', route => route.fulfill({ json: {
    profile: 'home.alice.smith.dhttp.net', owner_name: 'home.alice.smith.dhttp.net',
    badges: { pending_reviews: 2, incoming_contacts: 0 },
  } }))
  await page.route('**/std/workspace-api/settings/profile', route => route.fulfill({ json: {
    identity_name: 'home.alice.smith.dhttp.net', display_name: 'Alice Smith', avatar_url: null, updated_at: 0,
  } }))
  await page.route('**/std/workspace-api/libs', route => route.fulfill({ json: [journal] }))
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: [journal] }))
  await page.route('**/std/acl/access', route => route.fulfill({ json: {} }))
  await page.route('**/std/contacts?*', route => route.fulfill({ json: {
    items: [{ name: bob, alias: 'Bob', subject_id: 'bob', status: 'active' }], total: 1, page: 1, page_size: 100,
  } }))
})

test('cards, full detail, history, declared entry and language', async ({ page, context }, testInfo) => {
  await context.route('**/api/journal/page', route => route.fulfill({ contentType: 'text/html', body: '<h1>Journal</h1>' }))
  await page.goto('./apps?tab=mine&q=journal')
  await expect(page.getByRole('heading', { name: 'Personal journal' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Open ↗', exact: true })).toHaveAttribute('href', '/api/journal/page')
  await page.screenshot({ path: testInfo.outputPath('01-my-apps.png'), fullPage: true })
  const popup = page.waitForEvent('popup')
  await page.getByRole('link', { name: 'Open ↗', exact: true }).click()
  const app = await popup
  await expect(app.getByRole('heading', { name: 'Journal' })).toBeVisible()
  await app.close()
  await expect(page).toHaveURL(/tab=mine&q=journal$/)
  await page.getByRole('link', { name: 'Details', exact: true }).click()
  await expect(page).toHaveURL(/\/apps\/local\/journal\?/)
  await expect(page.locator('.app-about')).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('02-detail.png'), fullPage: true })
  await page.getByRole('button', { name: 'Interfaces', exact: true }).click()
  await expect(page.getByRole('link', { name: 'Open GET /api/journal/entries in a new tab' })).toBeVisible()
  await page.reload()
  await expect(page.getByRole('heading', { name: 'API endpoints (3)' })).toBeVisible()
  await page.goBack()
  await expect(page.locator('.app-about')).toBeVisible()
  await page.goForward()
  await expect(page.getByRole('heading', { name: 'API endpoints (3)' })).toBeVisible()
  await page.getByRole('link', { name: 'Back to apps' }).click()
  await expect(page).toHaveURL(/tab=mine&q=journal$/)
  await page.getByRole('button', { name: 'Chinese' }).click()
  await expect(page.getByRole('heading', { name: '应用管理' })).toBeVisible()
  await page.reload()
  await expect(page.getByRole('button', { name: '从文件安装' })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
})

test('live-shaped empty store, retry and update action preserve filters', async ({ page }, testInfo) => {
  let fail = true
  await page.route(storeApi + '/libs?*', route => {
    const query = new URL(route.request().url()).searchParams
    expect(query.get('q')).toBe('notes')
    expect(query.get('publisher')).toBe('alice.smith.dhttp.net')
    expect(query.get('page_size')).toBe('20')
    return fail ? route.fulfill({ status: 502, body: 'Store unavailable' })
      : route.fulfill({ json: { libs: [], total: 0, page: 1, page_size: 20 } })
  })
  await page.goto('./apps?tab=store&q=notes&publisher=alice.smith.dhttp.net')
  await expect(page.getByRole('alert')).toContainText('Store unavailable')
  fail = false
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(page.getByText('No matching apps', { exact: true })).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Publisher', exact: true })).toHaveValue('alice.smith.dhttp.net')
  await page.screenshot({ path: testInfo.outputPath('03-store-connected-empty.png'), fullPage: true })
  await page.getByRole('button', { name: 'Check for updates' }).click()
  await expect(page.getByText('No updates available', { exact: true })).toBeVisible()
  await expect(page.locator('.app-update-list')).toHaveCount(0)
  await expect(page.getByRole('heading', { name: 'Update check results', exact: true })).toHaveCount(0)
  await page.getByRole('button', { name: 'Dismiss message', exact: true }).click()
  await expect(page.getByText('No updates available', { exact: true })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Updates', exact: true })).toHaveCount(0)
  await page.reload()
  await expect(page.getByText('Updates have not been checked', { exact: true })).toHaveCount(0)
  await expect(page.locator('.app-update-list')).toHaveCount(0)
  await page.getByRole('navigation', { name: 'Primary navigation' }).getByRole('link', { name: 'Apps', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Personal journal' })).toBeVisible()
})

test('store details, exact release confirmation and native save activate without restarting', async ({ page }, testInfo) => {
  let disk = [completeJournal()]
  let live = [...disk]
  const writes: unknown[] = []
  const downloads: string[] = []
  page.on('request', req => { if (req.url().endsWith('/lib.webc')) downloads.push(req.url()) })
  await page.route('**/std/workspace-api/libs', route => route.fulfill({ json: live }))
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: disk }))
  await page.route(storeApi + '/libs?*', route => route.fulfill({ json: { libs: [summary()], total: 1, page: 1, page_size: 20 } }))
  await page.route(storeApi + `/libs/${fullId}/versions/1.2.0`, route => route.fulfill({ json: release() }))
  await page.route('**/std/pishoo/store-install', route => {
    expect(route.request().method()).toBe('POST')
    writes.push(route.request().postDataJSON())
    disk = [completeJournal('1.2.0')]
    live = [...disk]
    return route.fulfill({ json: disk[0] })
  })
  await page.goto('./apps?tab=store&q=journal')
  await page.getByRole('link', { name: 'Details', exact: true }).click()
  await expect(page).toHaveURL(/apps\/store\/alice.smith.dhttp.net\/journal\?.*version=1.2.0/)
  await page.getByRole('button', { name: 'Signature & host capabilities', exact: true }).click()
  await expect(page.getByText('b'.repeat(64), { exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Access permissions', exact: true })).toHaveCount(0)
  await page.getByRole('button', { name: 'Install v1.2.0', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toContainText(fullId)
  await expect(dialog).toContainText('Personal journal · v1.2.0')
  await expect(dialog.getByRole('button', { name: 'Confirm update', exact: true })).toBeDisabled()
  await dialog.getByRole('checkbox').check()
  await page.screenshot({ path: testInfo.outputPath('14-trusted-store-confirm.png'), fullPage: true })
  await dialog.getByRole('button', { name: 'Confirm update', exact: true }).click()
  await expect(dialog).toHaveCount(0)
  expect(writes).toEqual([{ publisher: 'alice.smith.dhttp.net', name: 'journal', version: '1.2.0' }])
  expect(downloads).toEqual([])
  const notice = page.locator('.app-notice-dismissible')
  await expect(notice).toHaveCount(1)
  await expect(notice).toContainText('Installed and ready to open')
  await notice.getByRole('button', { name: 'Dismiss message', exact: true }).click()
  await expect(notice).toHaveCount(0)
  await page.getByRole('button', { name: 'About', exact: true }).click()
  await expect(notice).toHaveCount(0)
  await page.getByRole('link', { name: 'Back to apps', exact: true }).click()
  await page.getByRole('button', { name: 'My apps', exact: true }).click()
  await expect(page.locator('.app-card')).not.toContainText('v1.1.0')
  await expect(page.locator('.app-card').getByRole('link', { name: 'Open ↗', exact: true })).toBeVisible()
  await expect(page.locator('.app-card')).toContainText('v1.2.0')
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
})

test('release history shows withdrawn versions and failed install keeps confirmation open', async ({ page }) => {
  await page.route(storeApi + `/libs/${fullId}/versions?*`, route => route.fulfill({ json: { versions: [summary(), summary('1.0.0', 'yanked')], page: 1, page_size: 20 } }))
  await page.route(storeApi + `/libs/${fullId}/versions/1.0.0`, route => route.fulfill({ json: release('1.0.0', 'yanked') }))
  await page.route(storeApi + `/libs/${fullId}/versions/1.2.0`, route => route.fulfill({ json: release() }))
  await page.route('**/std/pishoo/store-install', route => route.fulfill({ status: 400, body: 'Publisher signature verification failed' }))
  await page.goto(`./apps/store/${fullId}?tab=store&version=1.2.0&section=changes`)
  await expect(page.getByText('Withdrawn by publisher')).toBeVisible()
  await page.getByRole('link', { name: 'v1.0.0', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Install v1.0.0', exact: true })).toHaveCount(0)
  await page.getByRole('link', { name: 'v1.2.0', exact: true }).click()
  await page.getByRole('button', { name: 'Install v1.2.0', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await dialog.getByRole('checkbox').check()
  await dialog.getByRole('button', { name: 'Confirm installation', exact: true }).click()
  await expect(dialog.getByRole('alert')).toContainText('Publisher signature verification failed')
  await expect(page.getByText('Saved · restart Pishoo to activate', { exact: true })).toHaveCount(0)
})

test('update checks batch only saved complete IDs and preserve failure and absence results', async ({ page }, testInfo) => {
  const many = Array.from({ length: 105 }, (_, index) => ({ ...completeJournal(), id: `alice.smith.dhttp.net/app-${index}`, version: '1.0.0' }))
  const batches: Array<Array<{ publisher: string; name: string; version: string }>> = []
  await page.route('**/std/workspace-api/libs', route => route.fulfill({ json: [journal] }))
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: [journal, ...many, { id: 'broken', error: 'corrupt' }] }))
  await page.route(storeApi + '/updates', route => {
    const libs = route.request().postDataJSON().libs
    batches.push(libs)
    if (batches.length === 1) return route.fulfill({ status: 503, body: 'First batch unavailable' })
    return route.fulfill({ json: { libs: libs.map((lib: { publisher: string; name: string }, index: number) => ({ id: `${lib.publisher}/${lib.name}`, status: index === 0 ? 'not_found' : index === 1 ? 'unavailable' : 'current' })) } })
  })
  await page.goto('./apps')
  await page.getByRole('button', { name: 'Check for updates', exact: true }).click()
  await expect(page.locator('.app-update-list')).toHaveCount(0)
  await expect(page.locator('.app-notice-dismissible')).toContainText('Update check failed')
  await page.getByRole('button', { name: 'Dismiss message', exact: true }).click()
  await expect(page.locator('.app-notice-dismissible')).toHaveCount(0)
  expect(batches.map(batch => batch.length)).toEqual([100, 5])
  expect(batches.flat().every(lib => lib.publisher === 'alice.smith.dhttp.net' && lib.version === '1.0.0')).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('15-update-batch-results.png') })
})

test('update recommendations use the saved version and open an exact release confirmation', async ({ page }, testInfo) => {
  await page.route('**/std/workspace-api/libs', route => route.fulfill({ json: [completeJournal('1.0.0')] }))
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: [completeJournal('1.1.0')] }))
  await page.route(storeApi + '/updates', route => {
    expect(route.request().postDataJSON()).toEqual({ libs: [{ publisher: 'alice.smith.dhttp.net', name: 'journal', version: '1.1.0' }] })
    return route.fulfill({ json: { libs: [{ id: fullId, status: 'update', version: '1.2.0',
      detail_path: `/store/v1/libs/${fullId}/versions/1.2.0`,
      download_path: `/store/v1/libs/${fullId}/versions/1.2.0/lib.webc`, changelog: 'New release', host: {},
    }] } })
  })
  await page.route(storeApi + `/libs/${fullId}/versions/1.2.0`, route => route.fulfill({ json: release() }))
  await page.goto('./apps')
  await page.getByRole('button', { name: 'Check for updates', exact: true }).click()
  const results = page.getByRole('list', { name: 'Update check results', exact: true })
  await expect(results).toContainText('Update to v1.2.0')
  await expect(results.getByRole('listitem')).toHaveCount(1)
  await expect(results.locator('.app-card, .app-panel, .state-block')).toHaveCount(0)
  await page.screenshot({ path: testInfo.outputPath('inline-updates.png') })
  await results.getByRole('button', { name: 'Update to v1.2.0', exact: true }).click()
  await expect(page.getByRole('dialog')).toContainText('Personal journal · v1.2.0')
  await expect(page.getByRole('dialog')).toContainText('Saved version: 1.1.0')
})

test('loaded and saved versions stay separate, including removal and disk corruption', async ({ page }, testInfo) => {
  let disk: Array<LibSummary | { id: string; error: string }> = [{ ...journal, version: '1.2.0' }, { id: 'broken', error: 'missing lib.webc' }]
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: disk }))
  await page.route('**/std/pishoo/libs/journal', route => {
    expect(route.request().method()).toBe('DELETE')
    disk = disk.filter(lib => lib.id !== 'journal')
    return route.fulfill({ status: 204 })
  })
  await page.goto('./apps')
  const card = page.locator('.app-card').filter({ hasText: 'Personal journal' })
  await expect(card).toContainText('v1.1.0')
  await expect(card).toContainText('Saved · not active · v1.2.0')
  await expect(page.getByText('Saved component error: missing lib.webc')).toBeVisible()
  await card.getByRole('link', { name: 'Details', exact: true }).click()
  const notice = page.locator('.app-notice-dismissible')
  await expect(notice).toContainText('Saved · not active · v1.2.0')
  await notice.getByRole('button', { name: 'Dismiss message', exact: true }).click()
  await expect(notice).toHaveCount(0)
  await page.locator('.app-more > summary').click()
  await page.getByRole('button', { name: 'Uninstall', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toContainText('keeping its data and access rules')
  await page.screenshot({ path: testInfo.outputPath('04-uninstall.png'), fullPage: true })
  await dialog.getByRole('button', { name: 'Confirm uninstall' }).click()
  await expect(dialog).toHaveCount(0)
  await expect(notice).toHaveCount(1)
  await expect(notice).toContainText('Uninstalled')
  await notice.getByRole('button', { name: 'Dismiss message', exact: true }).click()
  await expect(notice).toHaveCount(0)
  await page.getByRole('link', { name: 'Back to apps', exact: true }).click()
  await expect(notice).toHaveCount(0)
  await expect(card).toContainText('Not saved locally')
  await expect(card.getByRole('link', { name: 'Open ↗' })).toBeVisible()
  await expect(card.getByRole('button', { name: 'Uninstall', exact: true })).toHaveCount(0)
})

test('legacy APIs are not guessed to be a page, disk-only apps cannot open', async ({ page }) => {
  await page.route('**/std/workspace-api/libs', route => route.fulfill({ json: [{ ...journal, openapi: undefined }] }))
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: [journal, { ...journal, id: 'new', title: 'New app' }] }))
  await page.goto('./apps')
  await expect(page.getByRole('link', { name: 'Open ↗', exact: true })).toHaveCount(0)
  await page.locator('.app-card').filter({ hasText: 'Personal journal' }).getByRole('link', { name: 'Details', exact: true }).click()
  await page.getByRole('button', { name: 'Interfaces', exact: true }).click()
  await expect(page.getByRole('link', { name: 'Open GET /api/journal/page in a new tab' })).toBeVisible()
})

test('declared root entry comes from the displayed release before activation', async ({ page }) => {
  const manifest: NonNullable<LibSummary['openapi']> = JSON.parse(readFileSync(
    new URL('../../examples/journal/openapi.json', import.meta.url), 'utf8',
  ))
  const app: LibSummary = {
    id: fullId, title: manifest.info.title, version: manifest.info.version,
    description: manifest.info.description ?? null, openapi: manifest,
    endpoints: Object.entries(manifest.paths ?? {}).flatMap(([path, methods]) =>
      Object.entries(methods).map(([method, operation]) => ({
        method: method.toUpperCase(), path: `/api/${fullId}${path}`,
        description: operation?.summary ?? null,
      })),
    ),
  }
  let saved: LibSummary[] = []
  await page.route('**/std/workspace-api/libs', route => route.fulfill({ json: [] }))
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: saved }))
  const storeRelease = { ...release(app.version), openapi: manifest }
  await page.route(storeApi + `/libs/${fullId}/versions/${app.version}`, route => route.fulfill({ json: storeRelease }))

  // A store release declares its entry even when it has never been installed.
  await page.goto(`./apps/store/${fullId}?version=${app.version}`)
  await expect(page.getByRole('heading', { name: app.title, exact: true })).toBeVisible()
  await expect(page.locator('.app-about')).not.toContainText('API app · no page entry declared')
  await expect(page.getByRole('link', { name: 'Open ↗', exact: true })).toHaveCount(0)

  // A saved release retains its entry while waiting for a restart to activate.
  saved = [app]
  await page.goto(`./apps/local/${fullId}`)
  await expect(page.getByRole('heading', { name: app.title, exact: true })).toBeVisible()
  await expect(page.locator('.app-about')).not.toContainText('API app · no page entry declared')
  await expect(page.getByRole('link', { name: 'Open ↗', exact: true })).toHaveCount(0)

  // The warning still appears when the displayed manifest has no page entry.
  saved = [{ ...app, openapi: { ...manifest, 'x-pishoo': { ...manifest['x-pishoo'], entry: undefined } } }]
  await page.reload()
  await expect(page.locator('.app-about')).toContainText('API app · no page entry declared')
})

test('local file inspection, update confirmation, save and failed save', async ({ page }, testInfo) => {
  let disk = [journal]
  let live = [...disk]
  await page.route('**/std/workspace-api/libs', route => route.fulfill({ json: live }))
  let fail = true
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: disk }))
  await page.route('**/std/pishoo/lib-check', route => route.fulfill({ json: { ...journal, version: '1.2.0' } }))
  await page.route('**/std/pishoo/libs/journal', route => {
    expect(route.request().headers()['content-type']).toBe('application/webc')
    if (fail) return route.fulfill({ status: 500, body: 'Compilation or save failed' })
    disk = [{ ...journal, version: '1.2.0' }]
    live = [...disk]
    return route.fulfill({ json: disk[0] })
  })
  await page.goto('./apps')
  await page.getByRole('button', { name: 'Update from file' }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog.getByLabel('Local component ID')).toHaveValue('journal')
  await dialog.getByLabel('WEBC package file').setInputFiles({ name: 'journal.webc', mimeType: 'application/webc', buffer: Buffer.from('fixture') })
  await expect(dialog.getByRole('button', { name: 'Confirm update' })).toBeDisabled()
  await dialog.getByRole('button', { name: 'Inspect component' }).click()
  await expect(dialog.getByRole('heading', { name: 'Personal journal · v1.2.0' })).toBeVisible()
  await dialog.getByRole('checkbox').check()
  await page.screenshot({ path: testInfo.outputPath('05-update-from-file.png'), fullPage: true })
  await dialog.getByRole('button', { name: 'Confirm update' }).click()
  await expect(dialog.getByRole('alert')).toContainText('result is uncertain')
  fail = false
  await dialog.getByRole('button', { name: 'Confirm update' }).click()
  await expect(dialog).toHaveCount(0)
  await expect(page.locator('.app-card')).not.toContainText('v1.1.0')
  await expect(page.locator('.app-card')).toContainText('v1.2.0')
  await page.getByRole('button', { name: 'Install from file' }).click()
  await dialog.getByLabel('Local component ID').fill('new')
  await page.screenshot({ path: testInfo.outputPath('06-install-from-file.png'), fullPage: true })
  await page.keyboard.press('Escape')
  await expect(dialog).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Install from file' })).toBeFocused()
})

test('three permission views use refreshed rules and partial results remain in dialog', async ({ page }, testInfo) => {
  const rules: RulesByApi = {}
  let fail = true
  const writes: Array<{ api: string; method: string; effect: string }> = []
  await page.route('**/std/acl/access', route => route.fulfill({ json: rules }))
  await page.route('**/std/acl/allow/*', route => {
    const change = route.request().postDataJSON()
    writes.push(change)
    if (fail && change.api.endsWith('/page')) return route.fulfill({ status: 409, body: 'daccess rejected this rule' })
    rules[change.api] ??= {}
    rules[change.api][change.method] = { allow: [], review: [], deny: [], [change.effect]: [bob] }
    return route.fulfill({ status: 204 })
  })
  await page.goto('./apps/local/journal?section=access')
  await expect(page.locator('.app-visitors')).toContainText(bob)
  await page.screenshot({ path: testInfo.outputPath('07-access-visitors.png'), fullPage: true })
  await page.getByRole('button', { name: 'Grant access', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await dialog.getByLabel('Scope', { exact: true }).click()
  await page.keyboard.press('Escape')
  await expect(dialog).toBeVisible()
  await expect(dialog.getByLabel('Scope', { exact: true })).toBeFocused()
  await dialog.getByLabel('Grantee', { exact: true }).fill(bob)
  await dialog.getByLabel('Scope', { exact: true }).selectOption('read-only')
  await expect(dialog).toContainText('Changes to submit (3)')
  await dialog.getByRole('button', { name: 'Save rule', exact: true }).click()
  await expect(dialog.getByRole('alert')).toContainText('daccess rejected this rule')
  await expect(dialog.getByRole('region', { name: 'Rule submission results' })).toContainText('Succeeded')
  await expect(dialog.getByRole('region', { name: 'Rule submission results' })).toContainText('Failed')
  await expect(dialog.getByRole('region', { name: 'Rule submission results' })).toContainText('Not executed')
  await page.screenshot({ path: testInfo.outputPath('08-access-partial-error.png'), fullPage: true })
  expect(writes).toHaveLength(2)
  fail = false
  await dialog.getByRole('button', { name: 'Save rule', exact: true }).click()
  await expect(dialog.getByRole('region', { name: 'Rule submission results' })).not.toContainText('Failed')
  expect(writes).toHaveLength(4) // Read-back avoids resubmitting the already saved entry rule.
  expect(writes[3]).toMatchObject({ api: '/api/journal/comments', effect: 'deny' })
  await dialog.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(page.locator('.app-visitors')).toContainText('deny')
  await page.getByRole('combobox', { name: 'Access permissions', exact: true }).selectOption('feature')
  await page.getByRole('button', { name: 'Reading 2', exact: true }).click()
  await expect(page.locator('.app-rule-list')).toHaveCount(2)
  await page.screenshot({ path: testInfo.outputPath('09-access-features.png'), fullPage: true })
  await page.getByRole('combobox', { name: 'Access permissions', exact: true }).selectOption('api')
  await page.getByRole('button', { name: 'POST /api/journal/comments', exact: true }).click()
  await expect(page.locator('.app-rule-list')).toContainText('deny')
  await page.screenshot({ path: testInfo.outputPath('10-access-api.png'), fullPage: true })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
})

test('empty catalogs and query failure are distinct and retry works', async ({ page }, testInfo) => {
  let fail = true
  await page.route('**/std/workspace-api/libs', route => fail ? route.fulfill({ status: 503, body: 'Runtime unavailable' }) : route.fulfill({ json: [] }))
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: [] }))
  await page.goto('./apps')
  await expect(page.getByRole('alert')).toContainText('Runtime unavailable')
  await expect(page.getByText('No apps installed', { exact: true })).toHaveCount(0)
  await page.screenshot({ path: testInfo.outputPath('11-query-error.png'), fullPage: true })
  fail = false
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(page.getByText('No apps installed', { exact: true })).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('12-empty.png'), fullPage: true })
})

test('invalid identities cannot construct component requests', async ({ page }) => {
  const reads: string[] = []
  page.on('request', request => { if (request.url().includes('/std/pishoo/libs/')) reads.push(request.url()) })
  await page.goto('./apps/local/alice.dhttp.net%2Fjournal?section=access')
  await expect(page.getByText('Invalid application identity')).toBeVisible()
  expect(reads).toEqual([])
})

test('card grid and long names fit every viewport', async ({ page }, testInfo) => {
  const legacy = { ...journal, id: 'archive', title: 'Personal research and reading archive', openapi: undefined }
  const pending = { ...journal, id: 'notebook', title: 'Notebook', version: '2.0.0', openapi: undefined }
  await page.route('**/std/workspace-api/libs', route => route.fulfill({ json: [journal, legacy] }))
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: [{ ...journal, version: '1.2.0' }, legacy, pending] }))
  await page.goto('./apps')
  await expect(page.locator('.app-card')).toHaveCount(3)
  await page.screenshot({ path: testInfo.outputPath('13-card-grid.png'), fullPage: true })
  const grid = await page.locator('.app-grid').evaluate(node => getComputedStyle(node).gridTemplateColumns.split(' ').length)
  expect(grid).toBe(testInfo.project.name === 'desktop' || testInfo.project.name === 'laptop' ? 2 : 1)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
})

test('permission editing pre-fills actual effects and removal keeps explicit denies', async ({ page }) => {
  const rules: RulesByApi = {
    '/api/journal/page': { GET: { allow: [bob], review: [], deny: [] } },
    '/api/journal/entries': { GET: { allow: [bob], review: [], deny: [] } },
    '/api/journal/comments': { POST: { allow: [], review: [], deny: [bob] } },
  }
  const deleted: string[] = []
  await page.route('**/std/acl/access', route => route.fulfill({ json: rules }))
  await page.route('**/std/acl/allow/*', route => {
    expect(route.request().method()).toBe('DELETE')
    const change = route.request().postDataJSON()
    deleted.push(change.api)
    delete rules[change.api][change.method]
    return route.fulfill({ status: 204 })
  })
  await page.goto('./apps/local/journal?section=access')
  await page.locator('.app-visitors').getByRole('button', { name: 'Edit', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog.getByLabel('Scope', { exact: true })).toHaveValue('read-only')
  await dialog.getByLabel('Scope', { exact: true }).selectOption('ungrant')
  await expect(dialog).toContainText('Changes to submit (2)')
  await dialog.getByRole('button', { name: 'Save rule', exact: true }).click()
  await expect(dialog.getByRole('region', { name: 'Rule submission results' })).toContainText('Succeeded')
  expect(deleted).toEqual(['/api/journal/entries', '/api/journal/page'])
  expect(rules['/api/journal/comments'].POST.deny).toEqual([bob])
})

// Bytes are passed intact to the host; package signature verification is server-side.
function uploadComponent(signed = true) {
  return { name: 'lib.webc', mimeType: 'application/webc', buffer: Buffer.from(signed ? 'signed WEBC fixture' : 'unsigned WEBC fixture') }
}

test('store upload rejects unsigned artifacts and a different publisher identity', async ({ page }) => {
  let checks = 0, uploads = 0
  await page.route(`${storeApi}/libs?*`, route => route.fulfill({ json: { libs: [], page: 1, page_size: 20, total: 0 } }))
  await page.route('**/std/pishoo/lib-check', route => { checks++; return checks === 1 ? route.fulfill({ status: 400, body: 'Missing publisher signature volume' }) : route.fulfill({ json: completeJournal() }) })
  await page.route(`${storeApi}/libs/${fullId}/versions/*`, route => { uploads++; return route.fulfill({ json: release() }) })
  await page.goto('./apps?tab=store')
  await page.getByRole('button', { name: 'Upload app', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await dialog.locator('input[type=file]').setInputFiles(uploadComponent(false))
  await dialog.getByRole('button', { name: 'Inspect component' }).click()
  await expect(dialog.getByRole('alert')).toContainText('Missing publisher signature volume')
  expect(checks).toBe(1)
  await dialog.locator('input[type=file]').setInputFiles(uploadComponent())
  await dialog.getByRole('button', { name: 'Inspect component' }).click()
  await expect(dialog.getByRole('alert')).toContainText('differs from this Workspace')
  await expect(dialog.getByRole('button', { name: 'Publish version', exact: true })).toBeDisabled()
  expect(checks).toBe(2)
  expect(uploads).toBe(0)
})

test('store upload confirms the package version, keeps a failed upload and retries exact bytes', async ({ page }, testInfo) => {
  let failed = true, uploads = 0
  const artifact = uploadComponent()
  await page.route('**/std/workspace-api/context', route => route.fulfill({ json: {
    profile: 'alice.smith.dhttp.net', owner_name: 'alice.smith.dhttp.net', badges: { pending_reviews: 0, incoming_contacts: 0 },
  } }))
  await page.route(`${storeApi}/libs?*`, route => route.fulfill({ json: { libs: [], page: 1, page_size: 20, total: 0 } }))
  await page.route('**/std/pishoo/lib-check', route => route.fulfill({ json: completeJournal() }))
  await page.route(`${storeApi}/libs/${fullId}/versions/1.1.0`, async route => {
    if (route.request().method() === 'PUT') {
      uploads++
      expect(route.request().headers()['content-type']).toBe('application/webc')
      expect(route.request().postDataBuffer()).toEqual(artifact.buffer)
      if (failed) return route.fulfill({ status: 409, body: 'This version already has different content' })
    }
    return route.fulfill({ json: release('1.1.0') })
  })
  await page.goto('./apps?tab=store')
  await page.getByRole('button', { name: 'Upload app', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await dialog.locator('input[type=file]').setInputFiles(artifact)
  await dialog.getByRole('button', { name: 'Inspect component' }).click()
  await expect(dialog.getByRole('heading', { name: 'Personal journal · v1.1.0' })).toBeVisible()
  await expect(dialog.getByRole('button', { name: 'Publish version', exact: true })).toBeDisabled()
  await dialog.getByRole('checkbox').check()
  await page.screenshot({ path: testInfo.outputPath('upload-confirmation.png'), fullPage: true })
  await dialog.getByRole('button', { name: 'Publish version', exact: true }).click()
  await expect(dialog.getByRole('alert')).toContainText('different content')
  await expect(dialog.getByRole('checkbox')).toBeChecked()
  failed = false
  await dialog.getByRole('button', { name: 'Publish version', exact: true }).click()
  await expect(page).toHaveURL(/apps\/store\/alice.smith.dhttp.net\/journal\?.*version=1.1.0/)
  await expect(dialog).not.toBeVisible()
  expect(uploads).toBe(2)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
})

test('preview folds business APIs, groups page files and enables Open immediately after install', async ({ page }) => {
  const manifest: NonNullable<LibSummary['openapi']> = JSON.parse(readFileSync(new URL('../../examples/journal/openapi.json', import.meta.url), 'utf8'))
  const installed: LibSummary = { id: fullId, title: manifest.info.title, version: manifest.info.version, description: manifest.info.description ?? null, openapi: manifest,
    endpoints: Object.entries(manifest.paths ?? {}).flatMap(([path, methods]) => Object.entries(methods).map(([method, operation]) => ({ method: method.toUpperCase(), path: `/api/${fullId}${path}`, description: operation?.summary ?? null }))),
  }
  let live: LibSummary[] = [], disk: LibSummary[] = []
  await page.route('**/std/workspace-api/libs', route => route.fulfill({ json: live }))
  await page.route('**/std/pishoo/libs', route => route.fulfill({ json: disk }))
  await page.route('**/std/pishoo/lib-check', route => route.fulfill({ json: installed }))
  await page.route(`**/std/pishoo/libs/${fullId}`, route => {
    if (route.request().method() === 'DELETE') { live = []; disk = []; return route.fulfill({ status: 204 }) }
    live = [installed]; disk = [installed]; return route.fulfill({ json: installed })
  })
  await page.goto('./apps')
  await page.getByRole('button', { name: 'Install from file', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await dialog.getByLabel('Local component ID').fill(fullId)
  await dialog.getByLabel('WEBC package file').setInputFiles({ name: 'journal.webc', mimeType: 'application/webc', buffer: Buffer.from('fixture') })
  await dialog.getByRole('button', { name: 'Inspect component' }).click()
  const preview = dialog.locator('details.app-preview-interfaces')
  await expect(preview.locator('.app-operations')).not.toBeVisible()
  await preview.locator('summary').click()
  await expect(preview.locator('.app-operations')).toBeVisible()
  await expect(preview).toContainText('Page resources (3 files)')
  await expect(preview).not.toContainText('/assets/app.js')
  await expect(preview).not.toContainText('/assets/app.css')
  await dialog.getByRole('checkbox').check()
  await dialog.getByRole('button', { name: 'Confirm installation', exact: true }).click()
  await expect(dialog).toHaveCount(0)
  const card = page.locator('.app-card').filter({ hasText: manifest.info.title })
  await expect(card.getByRole('link', { name: 'Open ↗', exact: true })).toHaveAttribute('href', `/api/${fullId}/`)
  await card.getByRole('button', { name: 'Uninstall', exact: true }).click()
  await page.getByRole('dialog').getByRole('button', { name: 'Confirm uninstall', exact: true }).click()
  await expect(card).toHaveCount(0)
})
