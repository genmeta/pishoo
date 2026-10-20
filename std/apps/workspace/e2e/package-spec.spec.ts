import { expect, test } from '@playwright/test'
import type { LibSummary, StoreDetail } from '../src/api/types'

const publisher = 'alice.dhttp.net'
const id = `${publisher}/workspace`
const dependencies = ['workspace', 'chat'].map(name => ({ publisher, name, version: '0.2.0' }))
const app: LibSummary = {
  kind: 'app', id, title: 'Workspace', version: '0.2.0', description: 'Independent frontend',
  license: 'Apache-2.0', entry: '/', changelog: 'Split Chat into an independent backend.', dependencies, endpoints: [],
}
const release: StoreDetail = {
  ...app, kind: 'app', publisher, name: 'workspace', publisher_owner_hash: 'a'.repeat(64),
  status: 'published', status_reason: '', published_at: 1791850000, package_sha256: 'b'.repeat(64), size: 4096,
  download_path: `/store/v1/apps/${id}/versions/0.2.0/app.webc`, signer_certificates: ['certificate'],
  description: app.description!, changelog: app.changelog!, host: null,
}

test.beforeEach(async ({ page }) => {
  await page.route('**/std/profile', route => route.fulfill({ json: { identity_name: publisher, display_name: null, avatar_url: null, updated_at: 0 } }))
  await page.route('**/std/acl/reviews?*', route => route.fulfill({ json: { items: [], total: 0, page: 1, page_size: 1 } }))
  await page.route('**/std/acl/capabilities', route => route.fulfill({ json: [] }))
  await page.route('**/std/acl/allow', route => route.fulfill({ json: {} }))
  await page.route('**/std/acl/access', route => route.fulfill({ json: {} }))
  await page.route('**/std/contacts?*', route => route.fulfill({ json: { items: [], total: 0, page: 1, page_size: 100 } }))
})

test('installed App opens its static entry and displays exact Lib dependencies without OpenAPI', async ({ page }) => {
  await page.route('**/std/sys/app/list', route => route.fulfill({ json: [app] }))
  await page.goto('/workspace/apps')
  await expect(page.getByRole('link', { name: 'Open ↗', exact: true })).toHaveAttribute('href', '/workspace/')
  await page.goto(`/workspace/apps/local/${id.split('/').at(-1)}?section=interfaces`)
  await expect(page.getByRole('heading', { name: 'Backend dependencies', exact: true })).toBeVisible()
  await expect(page.getByRole('link', { name: 'alice/chat', exact: true })).toHaveAttribute('href', /kind=lib.*version=0.2.0/)
  await expect(page.locator('.app-operations').getByText('v0.2.0', { exact: true })).toHaveCount(2)
})

test('store App preview sends a typed install request and lists automatically installed dependencies', async ({ page }) => {
  await page.route('**/std/sys/app/list', route => route.fulfill({ json: [] }))
  await page.route(`**/store/v1/apps/${id}/versions/0.2.0`, route => route.fulfill({ json: release }))
  await page.route(`**/store/v1/apps/${id}/versions?*`, route => route.fulfill({ json: { versions: [release], page: 1, page_size: 20 } }))
  let installed: unknown
  await page.route('**/std/sys/app?name=workspace', route => {
    expect(route.request().method()).toBe('PUT')
    expect(route.request().headers()['content-type']).toBe('application/json')
    installed = route.request().postDataJSON()
    return route.fulfill({ json: app })
  })
  await page.goto('/workspace/apps/store/alice/workspace?version=0.2.0')
  await expect(page.getByRole('heading', { name: 'Workspace', exact: true })).toBeVisible()
  await page.getByRole('button', { name: /Install v0.2.0/ }).click()
  const dialog = page.getByRole('dialog')
  await dialog.locator('details.app-preview-interfaces summary').click()
  await expect(dialog.getByRole('link', { name: 'alice/chat', exact: true })).toBeVisible()
  await dialog.getByRole('checkbox').check()
  await dialog.getByRole('button', { name: 'Confirm installation', exact: true }).click()
  await expect.poll(() => installed).toEqual({ publisher, version: '0.2.0' })
})
