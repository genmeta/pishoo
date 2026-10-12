import { expect, test } from '@playwright/test'
import type { Page } from '@playwright/test'

async function mockSettings(page: Page): Promise<void> {
  let displayName: string | null = 'Initial name'
  let avatarUrl: string | null = null

  await page.route('**/std/workspace-api/context', (route) => route.fulfill({ json: {
    profile: 'owner.local', owner_name: 'owner.local',
    badges: { pending_reviews: 0, incoming_contacts: null },
  } }))
  await page.route('**/std/acl/reviews?*', (route) => route.fulfill({ json: {
    items: [], total: 0, page: 1, page_size: 20,
  } }))
  await page.route('**/std/workspace-api/settings/profile', (route) => {
    if (route.request().method() === 'PATCH') {
      const body = route.request().postDataJSON() as { display_name: string }
      displayName = body.display_name.trim() || null
    }
    return route.fulfill({ json: {
      identity_name: 'owner.local', display_name: displayName, avatar_url: avatarUrl, updated_at: 1700000000,
    } })
  })
  await page.route('**/std/workspace-api/capabilities', (route) => route.fulfill({ json: [
    {
      id: 'public_profile', version: '1', visibility: 'public', approval_mode: 'none',
      selectable: false, endpoints: [{ method: 'GET', path: '/std/profile' }],
    },
    {
      id: 'chat', version: '1', visibility: 'contact', approval_mode: 'capability',
      selectable: true, endpoints: [{ method: 'POST', path: '/std/message' }],
    },
  ] }))
  await page.route(/\/std\/workspace-api\/settings\/profile\/avatar(?:\?.*)?$/, (route) => {
    if (route.request().method() === 'GET') {
      return route.fulfill({
        contentType: 'image/png',
        body: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=', 'base64'),
      })
    }
    avatarUrl = route.request().method() === 'DELETE' ? null : '/std/workspace-api/settings/profile/avatar'
    return route.fulfill({ json: {
      identity_name: 'owner.local', display_name: displayName, avatar_url: avatarUrl, updated_at: 1700000001,
    } })
  })
}

test.beforeEach(async ({ page }) => mockSettings(page))

test('public display name updates the profile header', async ({ page }) => {
  await page.goto('./settings/profile')
  await expect(page.getByRole('heading', { name: 'My profile' })).toBeVisible()
  await expect(page.getByRole('region', { name: 'My profile' }).getByText('owner.local', { exact: true })).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Display name' })).toHaveValue('Initial name')
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
  await page.getByRole('textbox', { name: 'Display name' }).fill('Updated name')
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.locator('.profile-summary strong')).toHaveText('Updated name')
  await expect(page.locator('.profile-summary small')).toHaveText('owner.local')
  await page.reload()
  await expect(page.locator('.profile-summary strong')).toHaveText('Updated name')
})

test('public avatar can be uploaded, previewed and removed', async ({ page }) => {
  await page.goto('./settings/profile')
  await page.getByLabel('Public avatar').setInputFiles({
    name: 'avatar.png',
    mimeType: 'image/png',
    buffer: Buffer.from('89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c489', 'hex'),
  })
  await expect(page.getByText('Avatar updated', { exact: true })).toBeVisible()
  await expect(page.locator('.profile-avatar-editor img')).toBeVisible()
  await page.getByRole('button', { name: 'Remove avatar' }).click()
  await expect(page.getByText('Avatar removed', { exact: true })).toBeVisible()
  await expect(page.locator('.profile-avatar-editor img')).toHaveCount(0)
})

test('capability catalog shows public and contact capability scopes', async ({ page }) => {
  await page.goto('./settings/capabilities')
  await expect(page.getByRole('heading', { name: 'Capabilities' })).toBeVisible()
  await expect(page.getByRole('article').filter({ hasText: 'One-to-one chat' })).toContainText('Capability grant required')
  await expect(page.getByRole('article').filter({ hasText: 'Public profile' })).toContainText('Public')
})

test('settings labels and errors are available in Chinese', async ({ page }) => {
  await page.goto('./settings/profile')
  await page.getByRole('button', { name: 'Chinese' }).click()
  await expect(page.getByRole('heading', { name: '我的信息' })).toBeVisible()
  await page.getByRole('textbox', { name: '显示名称' }).fill('😀'.repeat(81))
  await page.getByRole('button', { name: '保存', exact: true }).click()
  await expect(page.getByText('最多输入 80 个字符，不能包含控制字符。')).toBeVisible()
  await page.getByRole('textbox', { name: '显示名称' }).fill('😀'.repeat(80))
  await page.getByRole('button', { name: '保存', exact: true }).click()
  await expect(page.locator('.profile-summary strong')).toHaveText('😀'.repeat(80))
})
