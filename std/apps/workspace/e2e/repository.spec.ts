import { expect, test } from '@playwright/test'
import type { LibSummary, StoreDetail } from '../src/api/types'

const publisher = 'dev.dhttp.net'
const id = `${publisher}/workspace`
const sourceUrl = 'https://github.com/genmeta/workspace/tree/v0.2.0'
function summary(kind: 'app' | 'lib', repository?: string): LibSummary {
  return { id, kind, title: 'Workspace', version: '0.2.0', description: 'Package description',
    changelog: 'Initial release', license: 'Apache-2.0', dependencies: [], endpoints: [], repository,
    ...(kind === 'app' ? { entry: '/' } : { host: {}, openapi: { openapi: '3.1.0', info: { title: 'Workspace', version: '0.2.0' }, paths: {} } }) }
}
function release(kind: 'app' | 'lib', repository?: string): StoreDetail {
  return { ...summary(kind, repository), publisher, name: 'workspace', publisher_owner_hash: 'a'.repeat(64),
    status: 'published', status_reason: '', published_at: 1, package_sha256: 'b'.repeat(64), size: 4096,
    download_path: `/store/v1/${kind === 'app' ? 'apps' : 'libs'}/${id}/versions/0.2.0/${kind}.webc`, signer_certificates: [],
    description: 'Package description', changelog: 'Initial release', host: {} }
}

async function setup(page: import('@playwright/test').Page, kind: 'app' | 'lib', repository?: string, diskRepository = repository) {
  const collection = kind === 'app' ? 'apps' : 'libs'
  await page.route('**/api/workspace/context', route => route.fulfill({ json: { profile: publisher, owner_name: publisher, badges: { pending_reviews: 0, incoming_contacts: 0 } } }))
  await page.route('**/api/workspace/settings/profile', route => route.fulfill({ json: { identity_name: publisher, display_name: null, avatar_url: null, updated_at: 0 } }))
  await page.route('**/std/acl/access', route => route.fulfill({ json: {} }))
  await page.route('**/std/contacts?*', route => route.fulfill({ json: { items: [], total: 0, page: 1, page_size: 100 } }))
  await page.route(`**/std/sys/package/loaded-${collection}`, route => route.fulfill({ json: [summary(kind, repository)] }))
  await page.route(`**/std/sys/package/${collection}`, route => route.fulfill({ json: [summary(kind, diskRepository)] }))
  await page.route(`**/store/v1/${collection}/${id}/versions/0.2.0`, route => route.fulfill({ json: release(kind, sourceUrl) }))
}

for (const kind of ['app', 'lib'] as const) {
  for (const origin of ['local', 'store']) {
    test(`${origin} ${kind} details show their own source repository without fetching it`, async ({ page }) => {
      await setup(page, kind, sourceUrl)
      let sourceRequests = 0
      await page.route('https://github.com/**', route => { sourceRequests++; return route.abort() })
      await page.goto(`/#/apps/${origin}/${origin === 'local' ? id.split('/').at(-1) : id}?kind=${kind}&version=0.2.0`)
      const link = page.getByRole('link', { name: sourceUrl, exact: true })
      await expect(page.getByText('Source repository', { exact: true })).toBeVisible()
      await expect(link).toHaveAttribute('href', sourceUrl)
      await expect(link).toHaveAttribute('target', '_blank')
      await expect(link).toHaveAttribute('rel', 'noopener noreferrer')
      expect(sourceRequests).toBe(0)
    })
  }
}

for (const value of [undefined, 'javascript:alert(1)', 'data:text/html,<script>alert(1)</script>', 'http://github.com/a/b', '//github.com/a/b', 'https://', 'https://user:password@github.com/a/b', 'https://github.com\\evil/a']) {
  test(`source repository is hidden for ${String(value)}`, async ({ page }) => {
    await setup(page, 'app', value)
    await page.goto(`/#/apps/local/${id.split('/').at(-1)}`)
    await expect(page.getByRole('heading', { name: 'Workspace', exact: true })).toBeVisible()
    await expect(page.getByText('Source repository', { exact: true })).toHaveCount(0)
  })
}

test('a loaded package without a repository never borrows disk or store metadata', async ({ page }) => {
  await setup(page, 'app', undefined, 'https://github.com/genmeta/workspace/tree/newer')
  await page.goto(`/#/apps/local/${id.split('/').at(-1)}`)
  await expect(page.getByRole('heading', { name: 'Workspace', exact: true })).toBeVisible()
  await expect(page.getByText('Source repository', { exact: true })).toHaveCount(0)
})
