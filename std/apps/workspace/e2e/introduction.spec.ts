import { expect, test } from '@playwright/test'
import type { BrowserContext, Page } from '@playwright/test'

import type { Introduction } from '../src/api/types'

const introduction: Introduction = {
  identity_name: 'alice.dhttp.net', display_name: 'Alice Chen',
  avatar_url: '/std/profile/avatar', updated_at: 1700000000,
  viewer_name: 'bob.dhttp.net', is_owner: false,
}
// AnySee displays canonical DHTTP hosts using its trailing-tilde shorthand.
const viewerOrigin = /^https:\/\/bob(?:\.dhttp\.net|~)\//
const addFriendUrl = /^https:\/\/bob(?:\.dhttp\.net|~)\/std\/workspace\/#\/contacts\/new\?target=alice\.dhttp\.net$/
const avatar = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=',
  'base64',
)

async function mockIntroduction(page: Page, overrides: Partial<Introduction> = {}): Promise<void> {
  await page.route('**/std/workspace/intro/context', (route) => route.fulfill({ json: { ...introduction, ...overrides } }))
  await page.route('**/std/profile/avatar?*', (route) => route.fulfill({ contentType: 'image/png', body: avatar }))
}

async function mockWorkspace(page: Page | BrowserContext): Promise<void> {
  await page.route('**/std/dhttp/**/std/acl/capabilities', (route) => route.fulfill({ json: [] }))
  await page.route('**/std/workspace/context', (route) => route.fulfill({ json: {
    profile: 'bob.dhttp.net', owner_name: 'bob.dhttp.net',
    badges: { pending_reviews: 0, incoming_contacts: 0 },
  } }))
  await page.route('**/std/workspace/settings/profile', (route) => route.fulfill({ json: {
    identity_name: 'bob.dhttp.net', display_name: 'Bob', avatar_url: null, updated_at: 1700000000,
  } }))
  await page.route('**/std/acl/capabilities', (route) => route.fulfill({ json: [{
    id: 'dev.dhttp.net/message/message', title: 'One-to-one chat', version: '1', visibility: 'contact', approval_mode: 'capability',
    selectable: true, endpoints: [{ method: 'POST', path: '/std/message' }],
  }] }))
  await page.route('**/std/workspace/contact-requests?*', (route) => route.fulfill({ json: {
    items: [], total: 0, page: 1, page_size: 20,
  } }))
}

test.beforeEach(async ({ context }) => {
  await context.routeWebSocket('**', (socket) => socket.close())
})

test('public introduction opens the visitor workspace in a new tab and submits the canonical target', async ({ page, context }, testInfo) => {
  const frontendOrigin = new URL(testInfo.project.use.baseURL!).origin
  // Serve the other identity from the isolated dev server; never contact a real peer.
  await context.route(viewerOrigin, async (route) => {
    const url = new URL(route.request().url())
    const response = await route.fetch({ url: `${frontendOrigin}${url.pathname}${url.search}` })
    await route.fulfill({ response })
  })
  await mockIntroduction(page)
  await mockWorkspace(context)
  let submitted: unknown
  await context.route('**/std/workspace/contact-requests', (route) => {
    submitted = route.request().postDataJSON()
    return route.fulfill({ status: 201, json: { id: 1 } })
  })
  const adminRequests: string[] = []
  page.on('request', (request) => {
    const path = new URL(request.url()).pathname
    if (['fetch', 'xhr'].includes(request.resourceType()) && path.startsWith('/std/workspace/') && !['/std/workspace/intro/context', '/std/profile/avatar'].includes(path)) adminRequests.push(request.url())
  })

  await page.goto('/std/workspace/#/intro')
  await expect(page.getByRole('heading', { name: 'Alice Chen' })).toBeVisible()
  await expect(page.locator('.introduction-identity')).toHaveText('Identity name: alice')
  await expect(page.locator('.introduction-card img')).toBeVisible()
  expect(adminRequests).toEqual([])
  await expect(page.getByRole('link', { name: 'Add as bob' })).toHaveAttribute(
    'href', addFriendUrl,
  )
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('introduction.png'), fullPage: true })
  const introductionUrl = page.url()
  await page.getByRole('link', { name: 'Add as bob' }).focus()
  const workspaceOpened = context.waitForEvent('page')
  await page.keyboard.press('Enter')
  const workspace = await workspaceOpened
  await expect(workspace).toHaveURL(addFriendUrl)
  await expect(page).toHaveURL(introductionUrl)
  expect(await workspace.evaluate(() => window.opener === null)).toBe(true)
  await expect(workspace.getByRole('textbox', { name: 'Recipient DHTTP name' })).toHaveValue('alice')
  await workspace.reload()
  await expect(workspace.getByRole('textbox', { name: 'Recipient DHTTP name' })).toHaveValue('alice')
  await workspace.getByRole('textbox', { name: 'Introduction', exact: true }).fill('Hello Alice')
  await workspace.getByRole('button', { name: 'Send request' }).click()
  await expect(workspace.getByRole('heading', { name: 'Sent requests' })).toBeVisible()
  expect(submitted).toEqual({
    target_name: 'alice.dhttp.net', description: 'Hello Alice',
    requested_capabilities: [], offered_capabilities: [],
  })
})

test('anonymous visitors choose their own identity and cannot use unsafe links or add themselves', async ({ page, context }) => {
  await mockIntroduction(page, { viewer_name: null })
  await context.route(viewerOrigin, (route) => route.fulfill({
    contentType: 'text/html', body: '<h1>Visitor workspace</h1>',
  }))
  await page.goto('/std/workspace/#/intro/')
  await page.getByRole('button', { name: 'Add friend', exact: true }).click()
  const name = page.getByRole('textbox', { name: 'Your identity name' })
  await expect(name).toBeFocused()
  for (const invalid of ['bob.dhttp.net@evil.example', 'K']) {
    await name.fill(invalid)
    await page.getByRole('button', { name: 'Continue to add friend' }).click()
    await expect(page.getByRole('alert')).toHaveText('Enter a valid identity name.')
  }
  await name.fill('ALICE~')
  await page.getByRole('button', { name: 'Continue to add friend' }).click()
  await expect(page.getByRole('alert')).toHaveText('You cannot add your own identity.')
  await name.fill(' BOB~ ')
  const introductionUrl = page.url()
  const workspaceOpened = context.waitForEvent('page')
  await page.getByRole('button', { name: 'Continue to add friend' }).click()
  const workspace = await workspaceOpened
  await expect(workspace).toHaveURL(addFriendUrl)
  await expect(workspace.getByRole('heading', { name: 'Visitor workspace' })).toBeVisible()
  await expect(page).toHaveURL(introductionUrl)
  expect(await workspace.evaluate(() => window.opener === null)).toBe(true)
})

test('owner view offers editing and Chinese labels', async ({ page }) => {
  await mockIntroduction(page, { viewer_name: 'alice.dhttp.net', is_owner: true })
  await page.goto('/std/workspace/#/intro')
  await expect(page.getByRole('link', { name: 'Add as' })).toHaveCount(0)
  await page.getByRole('button', { name: 'Chinese' }).click()
  await expect(page.getByText('这是你的介绍页。')).toBeVisible()
  await expect(page.getByRole('link', { name: '编辑我的信息' })).toHaveAttribute('href', '/std/workspace/#/settings/profile')
  await expect(page.locator('.introduction-identity')).toHaveText('身份名称: alice')
})

test('missing names and broken avatars fall back without hiding the add action', async ({ page }) => {
  await mockIntroduction(page, { display_name: null })
  await page.route('**/std/profile/avatar?*', (route) => route.fulfill({ status: 404 }))
  await page.goto('/std/workspace/#/intro')
  await expect(page.getByRole('heading', { name: 'alice', exact: true })).toBeVisible()
  await expect(page.locator('.introduction-card .avatar')).toHaveText('A')
  await expect(page.locator('.introduction-card img')).toHaveCount(0)
  await expect(page.getByRole('link', { name: 'Add as bob' })).toBeVisible()
})

test('matching identity names do not override the server ownership check', async ({ page }) => {
  await mockIntroduction(page, { viewer_name: 'alice.dhttp.net', is_owner: false })
  await page.goto('/std/workspace/#/intro')
  await expect(page.getByRole('heading', { name: 'Alice Chen' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Edit my profile' })).toHaveCount(0)
  await page.getByRole('button', { name: 'Add friend', exact: true }).click()
  await page.getByRole('textbox', { name: 'Your identity name' }).fill('alice')
  await page.getByRole('button', { name: 'Continue to add friend' }).click()
  await expect(page.getByRole('alert')).toHaveText('You cannot add your own identity.')
})

test('public introduction retries loading failures and treats display names as text', async ({ page }) => {
  let attempts = 0
  await page.route('**/std/workspace/intro/context', (route) => {
    attempts += 1
    return attempts === 1
      ? route.fulfill({ status: 503, body: 'Temporarily unavailable' })
      : route.fulfill({ json: { ...introduction, avatar_url: null, display_name: '<script>alert(1)</script>' } })
  })
  await page.goto('/std/workspace/#/intro')
  await expect(page.getByRole('alert')).toContainText('Temporarily unavailable')
  await expect(page.getByRole('link', { name: 'Add as' })).toHaveCount(0)
  await page.getByRole('button', { name: 'Retry' }).click()
  await expect(page.getByRole('heading', { name: '<script>alert(1)</script>' })).toBeVisible()
  await expect(page.locator('.introduction-card script')).toHaveCount(0)
  expect(attempts).toBe(2)
})

test('long names fit and anonymous identity entry stays usable on narrow screens', async ({ page }) => {
  await mockIntroduction(page, {
    identity_name: `${'a'.repeat(63)}.${'b'.repeat(63)}.dhttp.net`,
    display_name: '很长的显示名称'.repeat(10), viewer_name: null, avatar_url: null,
  })
  await page.goto('/std/workspace/#/intro')
  await page.getByRole('button', { name: 'Chinese' }).click()
  await page.getByRole('button', { name: '添加好友', exact: true }).click()
  await expect(page.getByRole('textbox', { name: '你的身份名称' })).toBeFocused()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
  await expect(page.getByRole('button', { name: '继续添加好友' })).toBeVisible()
})

test('profile settings copy the introduction link and provide a manual fallback', async ({ page }, testInfo) => {
  await mockWorkspace(page)
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'clipboard', { value: {
      writeText: () => Promise.reject(new Error('Clipboard unavailable')),
    } })
  })
  await page.goto('/std/workspace/#/settings/profile')
  const share = page.getByRole('region', { name: 'Share my introduction' })
  const expected = new URL('/std/workspace/#/intro', page.url()).href
  await expect(share.getByRole('textbox', { name: 'Introduction link' })).toHaveValue(expected)
  await expect(share.getByRole('link', { name: 'Preview introduction' })).toHaveAttribute('href', expected)
  await share.getByRole('button', { name: 'Copy link' }).click()
  await expect(share.getByRole('status')).toHaveText('Select and copy the introduction link.')
  await expect(share.getByRole('textbox', { name: 'Introduction link' })).toBeFocused()
  expect(await share.getByRole('textbox').evaluate((input: HTMLInputElement) =>
    input.selectionEnd! - input.selectionStart!)).toBe(expected.length)
  await page.evaluate(() => {
    navigator.clipboard.writeText = async (value) => { document.body.dataset.copiedIntroduction = value }
  })
  await share.getByRole('button', { name: 'Copy link' }).click()
  await expect(share.getByRole('status')).toHaveText('Introduction link copied.')
  await expect(page.locator('body')).toHaveAttribute('data-copied-introduction', expected)
  await page.screenshot({ path: testInfo.outputPath('introduction-share.png'), fullPage: true })
})

test('prefilled targets reject self requests and show duplicate feedback', async ({ page }) => {
  await mockWorkspace(page)
  let submissions = 0
  await page.route('**/std/workspace/contact-requests', (route) => {
    submissions += 1
    return route.fulfill({ status: 409, body: 'pending request already exists' })
  })
  await page.goto('/std/workspace/#/contacts/new?target=bob.dhttp.net')
  await expect(page.getByRole('textbox', { name: 'Recipient DHTTP name' })).toHaveValue('bob')
  await page.getByRole('button', { name: 'Send request' }).click()
  await expect(page.getByRole('alert')).toHaveText('You cannot add your own identity.')
  expect(submissions).toBe(0)
  await page.getByRole('textbox', { name: 'Recipient DHTTP name' }).fill('alice')
  await page.getByRole('button', { name: 'Send request' }).click()
  await expect(page.getByRole('alert')).toHaveText('A pending request to this name already exists.')
  expect(submissions).toBe(1)
})

test('hash navigation switches between the public card and owner shell', async ({ page }) => {
  await mockIntroduction(page, { viewer_name: 'bob.dhttp.net', is_owner: true })
  await mockWorkspace(page)
  await page.goto('/std/workspace/#/intro')
  await expect(page.getByRole('heading', { name: 'Alice Chen' })).toBeVisible()
  await expect(page.locator('.app-shell')).toHaveCount(0)
  await page.getByRole('link', { name: 'Edit my profile' }).click()
  await expect(page.getByRole('region', { name: 'Share my introduction' })).toBeVisible()
  await expect(page.locator('.public-introduction')).toHaveCount(0)
  await page.goBack()
  await expect(page.getByRole('heading', { name: 'Alice Chen' })).toBeVisible()
  await expect(page.locator('.app-shell')).toHaveCount(0)
})
