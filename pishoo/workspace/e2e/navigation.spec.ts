import { expect, test } from '@playwright/test'

test.beforeEach(async ({ page }) => {
  await page.route('**/workspace-api/context', (route) => route.fulfill({ json: {
    profile: 'spike.liu.dhttp.net', owner_name: 'spike.liu.dhttp.net',
    badges: { pending_reviews: 2, incoming_contacts: null },
  } }))
  await page.route('**/workspace-api/settings/profile', (route) => route.fulfill({ json: {
    identity_name: 'spike.liu.dhttp.net', display_name: null, avatar_url: null, updated_at: 1700000000,
  } }))
  await page.route('**/workspace-api/profiles/*', (route) => route.fulfill({ json: {
    display_name: null, avatar_url: null, updated_at: 1700000000,
  } }))
  await page.route('**/acl/reviews?*', (route) => route.fulfill({ json: {
    items: [], total: 2, page: 1, page_size: 20,
  } }))
  await page.route('**/workspace-api/approvals?*', (route) => route.fulfill({ json: {
    items: [], total: 0, page: 1, page_size: 20,
  } }))
  await page.route('**/acl/apis?*', (route) => route.fulfill({ json: {
    items: [], total: 0, page: 1, page_size: 20,
  } }))
  await page.route('**/acl/access', (route) => route.fulfill({ json: {} }))
  await page.route('**/acl/allow', (route) => route.fulfill({ json: {} }))
  await page.route('**/workspace-api/contact-directory', (route) => route.fulfill({ json: [{
    name: 'alice.example', saved: false, chat_available: true, remote_chat_granted: true,
  }] }))
  await page.route('**/chat-api/conversations/alice.example/capability', (route) => route.fulfill({ json: {
    capability: 'chat', status: 'available', contact_status: 'active',
    can_send: true, can_receive: false, remote_grant: true,
    endpoints: [{ method: 'POST', path: '/std/message' }],
  } }))
  await page.route('**/contacts?*', (route) => route.fulfill({ json: {
    items: [{
      name: 'alice.example', subject_id: 'subject-alice', alias: null, class: 'Human',
      description: 'Test contact', status: 'active', created_at: 1700000000,
      updated_at: 1700000000, expired_after: 2000000000,
      requested_access: {}, granted_access: {}, offers: {},
    }], total: 1, page: 1, page_size: 20,
  } }))
  await page.route('**/contact/alice.example', (route) => route.fulfill({ json: {
    name: 'alice.example', subject_id: 'subject-alice', alias: null, class: 'Human',
    description: 'Test contact', status: 'active', created_at: 1700000000,
    updated_at: 1700000000, expired_after: 2000000000,
    requested_access: {}, granted_access: {}, offers: {},
  } }))
})

test('shell paths support navigation, refresh and browser history', async ({ page }, testInfo) => {
  await page.goto('./')
  await expect(page.getByRole('heading', { name: 'Workspace' })).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('home.png'), fullPage: true })
  const primary = page.getByRole('navigation', { name: 'Primary navigation' })
  await expect(primary.getByRole('link')).toHaveCount(5)
  await expect(primary.getByRole('link', { name: /Approval center/ })).toContainText('2')

  await primary.getByRole('link', { name: 'Contacts' }).focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/workspace\/contacts$/)
  await page.getByRole('button', { name: /alice\.example/ }).first().click()
  await expect(page).toHaveURL(/\/workspace\/contacts\/alice\.example$/)
  await expect(page.getByRole('complementary', { name: 'Contact details' })).toBeVisible()
  await page.reload()
  await expect(page.getByRole('complementary', { name: 'Contact details' })).toBeVisible()
  await page.goBack()
  await expect(page.getByRole('complementary', { name: 'Contact details' })).toHaveCount(0)
  await page.goForward()
  await expect(page.getByRole('complementary', { name: 'Contact details' })).toBeVisible()
  await page.getByRole('button', { name: 'Close contact details' }).click()

  await primary.getByRole('link', { name: 'Quick settings' }).click()
  await page.getByRole('link', { name: 'Access rules' }).click()
  await expect(page).toHaveURL(/\/workspace\/settings\/access$/)
  await expect(primary.getByRole('link', { name: 'Quick settings' })).toHaveAttribute('aria-current', 'page')
  await page.goto('./extensions')
  await expect(page.getByText('Not enabled yet')).toBeVisible()
  await expect(page.getByRole('button', { name: /Install|Enable|Chat/ })).toHaveCount(0)
})

test('five labeled navigation targets fit on a narrow screen', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'mobile')
  await page.goto('./apps')
  const primary = page.getByRole('navigation', { name: 'Primary navigation' })
  await expect(primary.getByRole('link')).toHaveCount(5)
  await expect(primary.getByRole('link', { name: 'Apps' })).toHaveAttribute('aria-current', 'page')
  await page.screenshot({ path: testInfo.outputPath('apps-mobile.png'), fullPage: true })
  const widths = await page.evaluate(() => ({
    viewport: document.documentElement.clientWidth,
    content: document.documentElement.scrollWidth,
    navLabelsFit: [...document.querySelectorAll<HTMLElement>('.primary-nav .nav-label-mobile')]
      .every((label) => label.scrollWidth <= label.clientWidth),
  }))
  expect(widths.content).toBeLessThanOrEqual(widths.viewport)
  expect(widths.navLabelsFit).toBe(true)
})

test('identity labels omit the domain suffix while routes and rule data retain it', async ({ page }) => {
  const name = 'friend.dhttp.net'
  const contact = {
    name, subject_id: 'friend-subject', alias: null, class: 'Human',
    description: 'Friend', status: 'active', created_at: 1700000000,
    updated_at: 1700000000, expired_after: 2000000000,
    requested_access: {}, granted_access: {}, offers: {},
  }
  await page.route('**/contacts?*', (route) => route.fulfill({ json: {
    items: [contact], total: 1, page: 1, page_size: 100,
  } }))
  await page.route('**/contact/friend.dhttp.net', (route) => route.fulfill({ json: contact }))
  await page.route('**/workspace-api/contact-directory', (route) => route.fulfill({ json: [{
    name, saved: false, chat_available: true, remote_chat_granted: true,
  }] }))
  await page.route('**/chat-api/conversations/friend.dhttp.net/capability', (route) => route.fulfill({ json: {
    capability: 'chat', status: 'available', contact_status: 'active',
    can_send: true, can_receive: false, remote_grant: true,
    endpoints: [{ method: 'POST', path: '/std/message' }],
  } }))
  await page.route('**/acl/reviews?*', (route) => route.fulfill({ json: {
    items: [{ id: 1, visitor: name, method: 'GET', api: '/files', reason: 'Need access', expired_after: '2033-01-01T00:00:00Z' }],
    total: 1, page: 1, page_size: 20,
  } }))
  await page.route('**/workspace-api/approvals?*', (route) => route.fulfill({ json: {
    items: [{ kind: 'access', id: 1, visitor: name, method: 'GET', api: '/files',
      reason: 'Need access', requested_at: 1700000000, expired_after: 2000000000 }],
    total: 1, page: 1, page_size: 20,
  } }))
  await page.route('**/acl/apis?*', (route) => route.fulfill({ json: {
    items: [{ api: '/files', updated_at: 1700000000 }], total: 1, page: 1, page_size: 20,
  } }))
  await page.route('**/acl/access', (route) => route.fulfill({ json: {
    '/files': { GET: { allow: [name], review: [], deny: [] } },
  } }))
  await page.route('**/acl/allow', (route) => route.fulfill({ json: {
    [name]: { '/files': { allow: ['GET'], review: [], deny: [] } },
  } }))

  await page.goto('./')
  await expect(page.locator('.profile-summary strong')).toHaveText('spike.liu')
  await expect(page.locator('.profile-summary small')).toHaveCount(0)
  await expect(page.locator('.overview-name')).toHaveText('spike.liu')
  await expect(page.locator('.workspace-overview')).not.toContainText('.dhttp.net')
  await page.goto('./settings/profile')
  await expect(page.getByRole('region', { name: 'My profile' }).locator('code')).toHaveText('spike.liu')

  await page.goto('./contacts')
  const link = page.locator('.contact-link').first()
  await expect(link).toContainText('friend')
  await expect(link).not.toContainText(name)
  await link.click()
  await expect(page).toHaveURL(/\/workspace\/contacts\/friend\.dhttp\.net$/)
  const drawer = page.getByRole('complementary', { name: 'Contact details' })
  await expect(drawer.getByRole('heading', { level: 2 })).toHaveText('friend')
  await expect(drawer.locator('.contact-heading strong')).toHaveText('friend')
  await page.goto('./settings/access?grantee=friend.dhttp.net')
  await expect(page).toHaveURL(/grantee=friend\.dhttp\.net$/)
  await expect(page.getByRole('heading', { name: 'friend' })).toBeVisible()
  await expect(page.locator('.master-list-items')).not.toContainText(name)
  await expect(page.getByRole('button', { name: 'Add rule' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Edit' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Delete' })).toHaveCount(0)
  await page.getByRole('tab', { name: 'By API' }).click()
  await expect(page.locator('tbody tr').first()).toContainText('friend')
  await expect(page.locator('tbody tr').first()).not.toContainText(name)

  await page.goto('./approvals')
  await expect(page.locator('tbody tr').first()).toContainText('friend')
  await expect(page.locator('tbody tr').first()).not.toContainText(name)
  await page.getByRole('button', { name: 'Allow' }).first().click()
  await expect(page.getByRole('dialog')).toContainText('friend')
  await expect(page.getByRole('dialog')).not.toContainText(name)
})
