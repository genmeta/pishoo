import { expect, test } from '@playwright/test'
import type { Page } from '@playwright/test'

import type { ChatMessage, Contact } from '../src/api/types'

function contact(name: string, status: Contact['status'], className = 'Human'): Contact {
  const chat = status === 'active' || status === 'pending' || status === 'transfered'
  return {
    name, status, class: className, subject_id: `subject-${name}`, alias: null,
    description: 'An incoming connection', created_at: 1700000000, updated_at: 1700000000,
    expired_after: 2000000000,
    requested_access: chat ? { '/std/message': ['POST'] } : { '/calendar': ['GET'] },
    granted_access: {}, offers: {},
  }
}

async function mockDirectory(page: Page): Promise<void> {
  const contactClasses = ['Human', 'Agent', 'Robot', 'Service'] as const
  const emptyActive = contact('empty-active.example', 'active')
  emptyActive.requested_access = {}
  const savedOnly = contact('saved-only.example', 'active')
  savedOnly.requested_access = {}
  const items: Contact[] = [
    ...Array.from({ length: 22 }, (_, index) =>
      contact(`active-${index}.example`, 'active', contactClasses[index % contactClasses.length])),
    emptyActive,
    contact('local-not-granted.example', 'active'),
    contact('local-unknown.example', 'active'),
    savedOnly,
    contact('saved-chat.example', 'active'),
    contact('saved-not-granted.example', 'active'),
    ...Array.from({ length: 100 }, (_, index) =>
      contact(`pending-${index}.example`, 'pending', contactClasses[index % contactClasses.length])),
    contact('expired.example', 'expired'),
    contact('transfer.example', 'transfered', 'Agent'),
    contact('blocked.example', 'blocked', 'Unknown class'),
  ]
  const saved = new Set(['saved-only.example', 'saved-chat.example', 'saved-not-granted.example'])
  const remoteGrants = new Set(items.filter((item) => item.name.startsWith('active-')
    || item.name === 'saved-chat.example').map((item) => item.name))
  const localGrants = new Set(['local-not-granted.example', 'local-unknown.example'])
  const deniedRemoteGrants = new Set(['local-not-granted.example', 'saved-not-granted.example'])
  const remoteGrant = (name: string) => remoteGrants.has(name) ? true
    : deniedRemoteGrants.has(name) ? false : null
  const chatAvailable = (item: Contact) => item.status === 'active'
    && (remoteGrants.has(item.name) || localGrants.has(item.name))
  await page.route('**/workspace-api/context', (route) => route.fulfill({ json: {
    profile: 'owner.local', owner_name: 'owner.local',
    badges: { pending_reviews: 0, incoming_contacts: null },
  } }))
  await page.route('**/workspace-api/settings/profile', (route) => route.fulfill({ json: {
    identity_name: 'owner.local', display_name: null, avatar_url: null, updated_at: 1700000000,
  } }))
  await page.route(/\/workspace-api\/profiles\/[^/?]+(?:\?.*)?$/, (route) => {
    const path = new URL(route.request().url()).pathname
    const name = decodeURIComponent(path.split('/').pop() ?? '')
    if (name === 'active-1.example') return route.fulfill({ status: 502, body: 'unavailable' })
    return route.fulfill({ json: {
      display_name: name === 'active-0.example' ? 'Remote Alice' : null,
      avatar_url: name === 'active-0.example'
        ? '/workspace-api/profiles/active-0.example/avatar?v=1700000000'
        : null,
      updated_at: 1700000000,
    } })
  })
  await page.route(/\/workspace-api\/profiles\/[^/?]+\/avatar(?:\?.*)?$/, (route) => route.fulfill({
    contentType: 'image/png',
    body: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=', 'base64'),
  }))
  await page.route('**/acl/reviews?*', (route) => route.fulfill({ json: {
    items: [], total: 0, page: 1, page_size: 20,
  } }))
  await page.route('**/workspace-api/approvals?*', (route) => {
    const url = new URL(route.request().url())
    const page = Number(url.searchParams.get('page') ?? 1)
    const pageSize = Number(url.searchParams.get('page_size') ?? 20)
    const expired = url.searchParams.get('status') === 'expired'
    const entries = expired ? [
      {
        kind: 'capability', request_id: 500, contact_name: 'expired-chat.example',
        capability_id: 'chat', capability_version: '1', requested_at: 1700000000,
        expired_after: 1700003600,
      },
      {
        kind: 'access', id: 501, visitor: 'expired-access.example', method: 'GET',
        api: '/files', reason: 'Needs approval', requested_at: 1700000000,
        expired_after: 1700003600,
      },
    ] : [
      {
        kind: 'access', id: 1, visitor: 'access.example', method: 'GET',
        api: '/files', reason: 'Needs approval', requested_at: 1700000000,
        expired_after: 2000000000,
      },
      ...items
        .filter((item) => ['pending', 'transfered'].includes(String(item.status))
          && Object.prototype.hasOwnProperty.call(item.requested_access, '/std/message'))
        .map((item) => ({
          kind: 'capability', request_id: Number(item.name.match(/\d+/)?.[0] ?? 0) + 1,
          contact_name: item.name, capability_id: 'chat', capability_version: '1',
          requested_at: item.updated_at, expired_after: item.expired_after,
        })),
    ]
    return route.fulfill({ json: {
      items: entries.slice((page - 1) * pageSize, page * pageSize),
      total: entries.length, page, page_size: pageSize,
    } })
  })
  await page.route('**/contacts?*', (route) => {
    const url = new URL(route.request().url())
    const current = Number(url.searchParams.get('page') ?? 1)
    const size = Number(url.searchParams.get('page_size') ?? 20)
    return route.fulfill({ json: {
      items: items.slice((current - 1) * size, current * size),
      total: items.length, page: current, page_size: size,
    } })
  })
  await page.route('**/workspace-api/contact-directory', (route) => route.fulfill({ json:
    items
      .filter((item) => saved.has(item.name) || item.status === 'blocked' || chatAvailable(item))
      .map((item) => ({
        name: item.name,
        saved: saved.has(item.name),
        chat_available: chatAvailable(item),
        remote_chat_granted: remoteGrant(item.name),
      })),
  }))
  await page.route(/\/workspace-api\/contacts\/[^/?]+\/saved$/, (route) => {
    const name = decodeURIComponent(new URL(route.request().url()).pathname.split('/').at(-2) ?? '')
    if (!items.some((item) => item.name === name)) return route.fulfill({ status: 404, body: 'not found' })
    if (route.request().method() === 'PUT') saved.add(name)
    else if (route.request().method() === 'DELETE') saved.delete(name)
    return route.fulfill({ status: 204, body: '' })
  })
  await page.route('**/contact/*', (route) => {
    const name = decodeURIComponent(new URL(route.request().url()).pathname.split('/').pop() ?? '')
    const found = items.find((item) => item.name === name)
    if (!found) return route.fulfill({ status: 404, body: 'not found' })
    if (route.request().method() === 'PATCH') Object.assign(found, route.request().postDataJSON())
    if (route.request().method() === 'DELETE') items.splice(items.indexOf(found), 1)
    return route.fulfill(route.request().method() === 'GET' ? { json: found } : { status: 204, body: '' })
  })
  await page.route('**/acl/apis?*', (route) => route.fulfill({ json: { items: [], total: 0, page: 1, page_size: 20 } }))
  await page.route('**/acl/access', (route) => route.fulfill({ json: {} }))
  await page.route('**/acl/allow', (route) => route.fulfill({ json: {} }))
  await page.route('**/workspace-api/contact-requests?*', (route) => route.fulfill({ json: {
    items: [], total: 0, page: 1, page_size: 20,
  } }))
  await page.route('**/workspace-api/capability-requests', (route) => route.fulfill({ json:
    items
      .filter((item) => ['pending', 'transfered'].includes(String(item.status))
        && Object.prototype.hasOwnProperty.call(item.requested_access, '/std/message'))
      .map((item) => ({
        request_id: Number(item.name.match(/\d+/)?.[0] ?? 0) + 1,
        contact_name: item.name,
        capability_id: 'chat',
        capability_version: '1',
        status: 'pending',
        requested_at: item.updated_at,
      })),
  }))
  await page.route(/\/chat-api\/conversations\/[^/]+\/capability$/, (route) => {
    const name = decodeURIComponent(new URL(route.request().url()).pathname.split('/')[3] ?? '')
    const found = items.find((item) => item.name === name)
    if (!found) return route.fulfill({ status: 404, body: 'not found' })
    const active = found.status === 'active'
    const canSend = active && (remoteGrants.has(name)
      || Object.prototype.hasOwnProperty.call(found.requested_access, '/std/message'))
    const canReceive = active && localGrants.has(name)
    return route.fulfill({ json: {
      capability: 'chat', status: found.status === 'blocked' ? 'blocked' : canSend || canReceive ? 'available' : 'waiting',
      contact_status: found.status, can_send: canSend, can_receive: canReceive,
      remote_grant: remoteGrant(name),
      endpoints: [{ method: 'POST', path: '/std/message' }],
    } })
  })
  await page.route(/\/workspace-api\/contacts\/[^/?]+\/capabilities\/chat\/grant(?:\?.*)?$/, (route) => {
    const url = new URL(route.request().url())
    const path = url.pathname.split('/')
    const name = decodeURIComponent(path[path.length - 4] ?? '')
    const found = items.find((item) => item.name === name)
    if (!found) return route.fulfill({ status: 404, body: 'not found' })
    const expectedId = Number(name.match(/\d+/)?.[0] ?? 0) + 1
    if (url.searchParams.has('request_id') && (
      Number(url.searchParams.get('request_id')) !== expectedId
      || url.searchParams.get('capability_version') !== '1'
    )) return route.fulfill({ status: 409, body: 'capability request changed' })
    found.status = 'active'
    localGrants.add(name)
    return route.fulfill({ status: 204, body: '' })
  })
  await page.route(/\/workspace-api\/contacts\/[^/?]+\/capabilities\/chat\/revoke$/, (route) => {
    const path = new URL(route.request().url()).pathname.split('/')
    const name = decodeURIComponent(path[path.length - 4] ?? '')
    localGrants.delete(name)
    return route.fulfill({ status: 204, body: '' })
  })
  await page.route(/\/workspace-api\/contacts\/[^/?]+\/capabilities\/chat\/deny(?:\?.*)?$/, (route) => {
    const url = new URL(route.request().url())
    const path = url.pathname.split('/')
    const name = decodeURIComponent(path[path.length - 4] ?? '')
    const found = items.find((item) => item.name === name)
    if (!found) return route.fulfill({ status: 404, body: 'not found' })
    const expectedId = Number(name.match(/\d+/)?.[0] ?? 0) + 1
    if (Number(url.searchParams.get('request_id')) !== expectedId
      || url.searchParams.get('capability_version') !== '1') {
      return route.fulfill({ status: 409, body: 'capability request changed' })
    }
    items.splice(items.indexOf(found), 1)
    return route.fulfill({ status: 204, body: '' })
  })
}

test.beforeEach(async ({ page }) => mockDirectory(page))

test('contact directory and sent requests use separate views', async ({ page }, testInfo) => {
  await page.goto('./contacts')
  await expect(page.getByText('22 contacts')).toBeVisible()
  const categories = page.getByRole('group', { name: 'Filter contacts by category' })
  await expect(categories.getByRole('button')).toHaveCount(3)
  await expect(categories.locator('[aria-pressed="true"]')).toHaveCount(0)
  await expect(page.getByRole('button', { name: /Needs attention\s+4/ })).toBeVisible()
  await expect(page.getByRole('button', { name: /Saved locally\s+3/ })).toBeVisible()
  await expect(page.getByRole('button', { name: /Blocked\s+1/ })).toBeVisible()
  await expect(page.getByRole('columnheader', { name: 'Updated' })).toHaveCount(0)
  await expect(page.getByRole('columnheader', { name: 'Expires' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Chat with active-0.example' })).toBeVisible()
  await expect(page.getByText('Remote Alice', { exact: true })).toBeVisible()
  await expect(page.locator('.contact-link').filter({ hasText: 'Remote Alice' })
    .locator('small')).toHaveText('active-0.example')
  await expect(page.locator('tbody')).not.toContainText('An incoming connection')
  await expect(page.getByText('active-1.example', { exact: true })).toBeVisible()
  for (const [name, className] of [
    ['active-1.example', 'Agent'],
    ['active-2.example', 'Robot'],
    ['active-3.example', 'Service'],
  ]) {
    await expect(page.locator('tbody tr').filter({ hasText: name })
      .getByRole('cell', { name: className, exact: true })).toBeVisible()
  }
  await expect(page.getByRole('button', { name: 'Chat with active-1.example' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Chat with active-2.example' })).toBeVisible()
  await expect(page.locator('.contact-link img')).toHaveCount(1)
  await expect(page.locator('tbody tr')).toHaveCount(20)
  const list = page.getByRole('region', { name: 'Contacts', exact: true })
  await expect(list.locator('.contact-status-note, .contact-saved-note')).toHaveCount(0)
  await expect(page.getByText('pending-0.example')).toHaveCount(0)
  await page.getByRole('button', { name: 'Next page' }).click()
  await expect(page.locator('tbody tr')).toHaveCount(2)
  await expect(page.getByText('blocked.example', { exact: true })).toHaveCount(0)
  await expect(page.getByText('empty-active.example', { exact: true })).toHaveCount(0)
  await expect(list).not.toContainText('local-not-granted.example')
  await expect(list).not.toContainText('local-unknown.example')
  await expect(list).not.toContainText('saved-only.example')
  await expect(list).not.toContainText('saved-chat.example')
  await expect(list).not.toContainText('saved-not-granted.example')

  await page.route('**/workspace-api/capabilities', (route) => route.fulfill({ json: [] }))
  await expect(page.getByRole('navigation', { name: 'Section navigation' })).toHaveCount(0)
  await page.setViewportSize({ width: 375, height: 812 })
  await expect(page.getByRole('button', { name: 'Add contact' })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('contacts-mobile-add-button.png') })
  await page.getByRole('button', { name: 'Add contact' }).click()
  await expect(page.getByRole('heading', { name: 'Add contact' })).toBeVisible()
  await expect(page).toHaveURL(/\/workspace\/contacts\/new$/)
  await expect(page.getByRole('navigation', { name: 'Section navigation' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Sent requests' })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('contact-new-mobile-sent-requests.png') })
  await page.setViewportSize({ width: 1440, height: 900 })
  const headerBox = await page.locator('.contact-new-header').boundingBox()
  const formBox = await page.locator('.outbound-form').boundingBox()
  expect(headerBox).not.toBeNull()
  expect(formBox).not.toBeNull()
  expect(Math.abs(headerBox!.x + headerBox!.width - formBox!.x - formBox!.width)).toBeLessThan(2)
  await page.screenshot({ path: testInfo.outputPath('contact-new-desktop-sent-requests.png') })
  await page.setViewportSize({ width: 768, height: 900 })
  const tabletHeaderBox = await page.locator('.contact-new-header').boundingBox()
  const tabletFormBox = await page.locator('.outbound-form').boundingBox()
  expect(tabletHeaderBox).not.toBeNull()
  expect(tabletFormBox).not.toBeNull()
  expect(Math.abs(tabletHeaderBox!.x + tabletHeaderBox!.width
    - tabletFormBox!.x - tabletFormBox!.width)).toBeLessThan(2)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
  await page.getByRole('button', { name: 'Sent requests' }).click()
  await expect(page.getByRole('heading', { name: 'Sent requests' })).toBeVisible()
  await expect(page).toHaveURL(/\/workspace\/contacts\/requests$/)
  await expect(page.getByText('No sent requests')).toBeVisible()
  await expect(page.locator('tbody tr')).toHaveCount(0)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('requests.png'), fullPage: true })
})

test('approval requester opens contact details and grants requested chat access', async ({ page }) => {
  await page.goto('./approvals')
  await page.getByRole('button', { name: /pending-0\.example/ }).first().click()
  await expect(page).toHaveURL(/\/workspace\/contacts\/pending-0\.example\?from=approvals$/)
  await expect(page.getByRole('combobox', { name: 'Local permission set' })).toHaveCount(0)
  await page.reload()
  await expect(page.getByRole('complementary', { name: 'Contact details' })).toContainText('pending-0.example')
  await page.getByRole('button', { name: 'Grant chat', exact: true }).click()
  await expect(page.getByRole('complementary', { name: 'Contact details' })).toContainText('Active')
  await expect(page.getByText('Chat delivery granted', { exact: true })).toBeVisible()
  await expect(page.getByRole('complementary', { name: 'Contact details' })
    .getByRole('button', { name: 'Open chat' })).toHaveCount(0)
  await page.getByRole('button', { name: 'Close contact details' }).click()
  await expect(page).toHaveURL(/\/workspace\/approvals$/)
  await page.goto('./contacts')
  await page.getByRole('button', { name: /Needs attention\s+5/ }).click()
  await expect(page.getByRole('button', { name: 'Chat with pending-0.example' })).toBeVisible()
})

test('saving an identity adds it to Contacts without granting Chat', async ({ page }) => {
  await page.goto('./contacts')
  await expect(page.getByText('22 contacts')).toBeVisible()
  await page.goto('./approvals')
  await page.getByRole('button', { name: /pending-0\.example/ }).first().click()
  const drawer = page.getByRole('complementary', { name: 'Contact details' })
  await expect(drawer.locator('.metadata-grid').filter({ hasText: 'Introduction' }))
    .toContainText('An incoming connection')
  await drawer.getByRole('button', { name: 'Save identity' }).click()
  await expect(page.getByText('Identity saved to Contacts', { exact: true })).toBeVisible()
  await expect(drawer.locator('.contact-heading')).toContainText('Saved locally')
  await expect(drawer.getByRole('button', { name: 'Grant chat', exact: true })).toBeVisible()
  await page.goto('./contacts')
  await expect(page.getByText('22 contacts')).toBeVisible()
  await page.getByRole('button', { name: /Saved locally\s+4/ }).click()
  await expect(page.getByText('4 contacts')).toBeVisible()
  await expect(page.getByText('pending-0.example')).toBeVisible()
  await page.getByRole('button', { name: /pending-0\.example/ }).first().click()
  await page.getByRole('complementary', { name: 'Contact details' })
    .getByRole('button', { name: 'Remove saved identity' }).click()
  await expect(page.getByText('3 contacts')).toBeVisible()
  await expect(page.getByRole('region', { name: 'Contacts', exact: true })).not.toContainText('pending-0.example')
})

test('contact categories share one list and keep identity state visible', async ({ page }) => {
  await page.goto('./contacts')
  await expect(page.getByText('22 contacts')).toBeVisible()
  await page.getByRole('button', { name: 'Next page' }).click()
  await page.getByRole('checkbox', { name: 'Select active-20.example', exact: true }).check()
  await page.getByRole('button', { name: /Needs attention\s+4/ }).click()
  const list = page.getByRole('region', { name: 'Contacts', exact: true })
  await expect(page.getByText('4 contacts')).toBeVisible()
  await expect(list.getByRole('checkbox', { checked: true })).toHaveCount(0)
  await expect(page.getByRole('button', { name: /Needs attention\s+4/ })).toHaveAttribute('aria-pressed', 'true')
  await expect(list.locator('tbody tr')).toHaveCount(4)
  await expect(list).toContainText('empty-active.example')
  await expect(list).toContainText('Chat send permission not granted')
  await expect(list).toContainText('Chat send permission unconfirmed')
  await expect(list).toContainText('local-not-granted.example')
  await expect(list).toContainText('local-unknown.example')
  await expect(list).toContainText('saved-not-granted.example')
  await expect(list.locator('.status-active')).toHaveCount(0)
  await list.getByRole('button', { name: 'Save identity' }).click()
  await expect(page.getByText('3 contacts')).toBeVisible()
  await page.getByRole('button', { name: /Saved locally\s+4/ }).click()
  await expect(list.locator('tbody tr')).toHaveCount(4)
  await expect(list).toContainText('empty-active.example')
  await expect(list).toContainText('saved-only.example')
  await expect(list).toContainText('saved-chat.example')
  await expect(list).toContainText('saved-not-granted.example')
  await expect(list.getByRole('button', { name: 'Chat with saved-chat.example' })).toBeVisible()
  await expect(list.locator('.status-active')).toHaveCount(0)
  await page.getByRole('button', { name: /Blocked\s+1/ }).click()
  await expect(list.locator('tbody tr')).toHaveCount(1)
  await expect(list).toContainText('blocked.example')
  await page.getByRole('button', { name: /Blocked\s+1/ }).click()
  await expect(page.getByRole('group', { name: 'Filter contacts by category' })
    .locator('[aria-pressed="true"]')).toHaveCount(0)
  await expect(page.getByText('22 contacts')).toBeVisible()
  await page.getByRole('button', { name: /Saved locally\s+4/ }).click()
  await page.getByRole('button', { name: /empty-active\.example/ }).first().click()
  await page.getByRole('complementary', { name: 'Contact details' })
    .getByRole('button', { name: 'Remove saved identity' }).click()
  await expect(page.getByText('3 contacts')).toBeVisible()
  await expect(page.getByRole('button', { name: /Needs attention\s+4/ })).toBeVisible()
  await page.getByRole('button', { name: 'Close contact details', exact: true }).click()
  await page.getByRole('button', { name: /Needs attention\s+4/ }).click()
  await page.getByRole('button', { name: /Needs attention\s+4/ }).click()
  await expect(page.getByText('22 contacts')).toBeVisible()
  await expect(list.locator('tbody tr')).toHaveCount(20)
})

test('approval center combines pending requests and shows expired requests without actions', async ({ page }) => {
  await page.goto('./approvals')
  await expect(page.getByRole('heading', { name: 'Approval center' })).toBeVisible()
  const pending = page.getByRole('region', { name: 'Pending approvals' })
  await expect(pending).toContainText('pending-0.example')
  await expect(pending).toContainText('Capability requests')
  await expect(pending).toContainText('Access approvals')
  await expect(pending).not.toContainText('Needs approval')
  await expect(page.getByText('102 pending', { exact: true })).toBeVisible()
  await pending.locator('tbody tr').filter({ hasText: 'pending-0.example' })
    .getByRole('button', { name: 'Allow', exact: true }).click()
  await expect(page.getByText('Chat delivery granted', { exact: true })).toBeVisible()
  await expect(pending).not.toContainText('pending-0.example')
  await pending.locator('tbody tr').filter({ hasText: 'pending-1.example' })
    .getByRole('button', { name: 'Deny', exact: true }).click()
  await expect(page.getByText('Chat request denied', { exact: true })).toBeVisible()
  await expect(pending).not.toContainText('pending-1.example')
  await page.getByRole('button', { name: 'Expired', exact: true }).click()
  const expired = page.getByRole('region', { name: 'Expired approvals' })
  await expect(expired).toContainText('expired-chat.example')
  await expect(expired).toContainText('expired-access.example')
  await expect(expired.getByRole('button', { name: /Grant|Deny|Allow/ })).toHaveCount(0)
  await page.getByRole('button', { name: 'Pending', exact: true }).click()
  await expect(pending).toContainText('access.example')
  await pending.locator('tbody tr').filter({ hasText: 'access.example' })
    .getByRole('button', { name: 'Allow', exact: true }).click()
  await expect(page.getByRole('dialog', { name: 'Allow request #1' })).toContainText('/files')
})

test('contact state and deletion remain available in the separated directory', async ({ page }) => {
  await page.goto('./contacts')
  await page.getByRole('button', { name: /active-0\.example/ }).first().click()
  const drawer = page.getByRole('complementary', { name: 'Contact details' })
  await expect(drawer.getByRole('button', { name: 'Apply permission set' })).toHaveCount(0)
  await drawer.getByRole('button', { name: 'Block' }).click()
  await expect(drawer).toContainText('Blocked')
  await expect(drawer.locator('.capability-direction-row .status')).toHaveText(['Unavailable', 'Unavailable'])
  await drawer.getByRole('button', { name: 'Restore' }).click()
  await expect(drawer).toContainText('Active')
  await drawer.getByRole('button', { name: 'Delete' }).click()
  const confirm = page.getByRole('dialog', { name: 'Delete contacts (1)' })
  await expect(confirm).toContainText('exact-subject access rules')
  await confirm.getByRole('button', { name: 'Delete' }).click()
  await expect(drawer).toHaveCount(0)
  await expect(page.getByText('21 contacts')).toBeVisible()
})

test('contact details show the other profile grant state without explanatory paragraphs', async ({ page }) => {
  await page.route(/\/chat-api\/conversations\/active-0\.example\/capability$/, (route) => route.fulfill({ json: {
    capability: 'chat', status: 'available', contact_status: 'active',
    can_send: true, can_receive: true, remote_grant: false,
    endpoints: [{ method: 'POST', path: '/std/message' }],
  } }))
  await page.goto('./contacts')
  await page.getByRole('button', { name: /active-0\.example/ }).first().click()
  const outgoing = page.getByRole('complementary', { name: 'Contact details' })
    .locator('.capability-direction-row').filter({ hasText: 'Me → other profile' })
  await expect(outgoing).toContainText('Not granted by other profile')
  await expect(outgoing.locator('p')).toHaveCount(0)
})

test('contact and approval details agree on local aliases and unknown chat grants', async ({ page }) => {
  const name = 'active-0.example'
  const subjectId = `hex:${Buffer.from(Array.from({ length: 64 }, (_, index) => index + 128)).toString('hex')}`
  const item = { ...contact(name, 'active'), alias: 'Local Alice', subject_id: subjectId }
  await page.route('**/contact/active-0.example', (route) => route.fulfill({ json: item }))
  await page.route(/\/chat-api\/conversations\/active-0\.example\/capability$/, (route) => route.fulfill({ json: {
    capability: 'chat', status: 'available', contact_status: 'active',
    can_send: true, can_receive: true, remote_grant: null,
    endpoints: [{ method: 'POST', path: '/std/message' }],
  } }))
  await page.route('**/workspace-api/approvals?*', (route) => route.fulfill({ json: {
    items: [{
      kind: 'capability', request_id: 900, contact_name: name,
      capability_id: 'chat', capability_version: '1',
      requested_at: 1700000000, expired_after: 2000000000,
    }], total: 1, page: 1, page_size: 20,
  } }))

  await page.goto(`./contacts/${name}`)
  const drawer = page.getByRole('complementary', { name: 'Contact details' })
  await expect(drawer.locator('.contact-heading img')).toBeVisible()
  await expect(drawer.locator('.contact-heading strong')).toHaveText('Local Alice')
  await expect(drawer.locator('.metadata-grid code')).toHaveText(subjectId)
  expect(await drawer.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true)
  await expect(drawer.locator('.capability-direction-row .status')).toHaveText(['Enabled', 'Not confirmed'])
  const states = await drawer.locator('.capability-direction-row .status').allTextContents()
  const summary = await drawer.locator('.contact-capability-card .status').textContent()

  await page.goto('./approvals')
  await page.getByRole('button', { name, exact: true }).click()
  await expect(drawer.locator('.contact-heading img')).toBeVisible()
  await expect(drawer.locator('.contact-heading strong')).toHaveText('Local Alice')
  await expect(drawer.locator('.metadata-grid code')).toHaveText(subjectId)
  expect(await drawer.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true)
  await expect(drawer.locator('.capability-direction-row .status')).toHaveText(states)
  await expect(drawer.locator('.contact-capability-card .status')).toHaveText(summary ?? '')
})

for (const status of [502, 504]) {
  test(`unavailable remote profiles (${status}) preserve approval details and chat`, async ({ page }) => {
    const name = 'active-0.example'
    const alias = 'Local Alice'
    await page.route(/\/workspace-api\/profiles\/[^/?]+(?:\?.*)?$/, (route) =>
      route.fulfill({ status, body: 'remote profile unavailable' }))
    await page.route('**/contact/active-0.example', (route) =>
      route.fulfill({ json: { ...contact(name, 'active'), alias } }))
    await page.route('**/workspace-api/approvals?*', (route) => route.fulfill({ json: {
      items: [{
        kind: 'capability', request_id: 900, contact_name: name,
        capability_id: 'chat', capability_version: '1',
        requested_at: 1700000000, expired_after: 2000000000,
      }], total: 1, page: 1, page_size: 20,
    } }))
    await page.route(/\/chat-api\/conversations\/active-0\.example\/messages\?/, (route) =>
      route.fulfill({ json: { items: [], next_cursor: null } }))

    await page.goto('./approvals')
    const profileFailure = page.waitForResponse((response) => response.url().endsWith(`/profiles/${name}`))
    await page.getByRole('button', { name, exact: true }).click()
    await profileFailure
    const drawer = page.getByRole('complementary', { name: 'Contact details' })
    await expect(drawer.locator('.contact-heading strong')).toHaveText(alias)
    await expect(drawer.locator('.metadata-grid code')).toHaveText(`subject-${name}`)
    await expect(drawer.locator('.capability-direction-row')).toHaveCount(2)

    const chatProfileFailure = page.waitForResponse((response) => response.url().endsWith(`/profiles/${name}`))
    await page.goto(`./contacts/${name}/chat`)
    await chatProfileFailure
    await expect(page.locator('main h1')).toHaveText(alias)
    const draft = page.locator('.chat-composer textarea')
    await expect(draft).toBeVisible()
    await draft.fill('Keep this as a local draft')
    await expect(page.locator('.chat-composer button[type="submit"]')).toBeEnabled()
    await expect(page.locator('.state-error')).toHaveCount(0)
  })
}

test('active contacts open the static chat page and expose message actions', async ({ page }) => {
  const messages: ChatMessage[] = [
    {
      id: 'incoming-1', client_message_id: 'remote-1', remote_message_id: 'remote-1',
      direction: 'incoming', state: 'received', sender: 'active-0.example', recipient: 'owner.local',
      text: 'Hello from the other side', created_at: 1700000000, updated_at: 1700000000,
      error_message: null,
    },
    {
      id: 'failed-1', client_message_id: 'local-1', remote_message_id: null,
      direction: 'outgoing', state: 'failed', sender: 'owner.local', recipient: 'active-0.example',
      text: 'Background delivery failed', created_at: 1700000010, updated_at: 1700000010,
      error_message: 'Temporary remote failure',
    },
  ]

  await page.route(/\/chat-api\/conversations\/active-0\.example\/capability$/, (route) => route.fulfill({
    json: {
      capability: 'chat', status: 'available', contact_status: 'active',
      can_send: true, can_receive: true, remote_grant: true,
      endpoints: [{ method: 'POST', path: '/std/message' }],
    },
  }))
  await page.route(/\/chat-api\/conversations\/active-0\.example\/messages(?:\?.*)?$/, async (route) => {
    if (route.request().method() === 'POST') {
      const body = route.request().postDataJSON() as { text: string }
      const message: ChatMessage = {
        id: `sent-${messages.length + 1}`, client_message_id: `local-${messages.length + 1}`,
        remote_message_id: null, direction: 'outgoing', state: 'sent', sender: 'owner.local',
        recipient: 'active-0.example', text: body.text, created_at: 1700000020,
        updated_at: 1700000020, error_message: null,
      }
      messages.push(message)
      return route.fulfill({ status: 202, json: message })
    }
    return route.fulfill({ json: { items: messages, next_cursor: null } })
  })
  await page.goto('./contacts')
  await page.getByRole('button', { name: 'Chat with active-0.example' }).click()
  await expect(page).toHaveURL(/\/workspace\/contacts\/active-0\.example\/chat$/)
  await expect(page.getByRole('heading', { name: 'Remote Alice' })).toBeVisible()
  const incoming = page.getByRole('article', { name: 'Incoming message' })
  await expect(incoming).toContainText('Hello from the other side')
  await expect(incoming.getByRole('img')).toHaveCount(0)
  await expect(page.locator('.chat-message time, .chat-message-meta')).toHaveCount(0)

  const failed = page.getByRole('article', { name: 'Your message' }).filter({ hasText: 'Background delivery failed' })
  const failureIcon = failed.getByRole('img', { name: 'Failed: Temporary remote failure' })
  await expect(failureIcon).toBeVisible()
  await expect(failureIcon).toHaveAttribute('title', 'Temporary remote failure')
  await expect(failed.locator('.chat-delivery + .chat-message-bubble')).toHaveCount(1)
  await expect(failed.locator('.chat-message-bubble')).not.toContainText('Temporary remote failure')
  await expect(page.getByRole('button', { name: 'Retry' })).toHaveCount(0)

  const composer = page.getByRole('textbox', { name: 'Message' })
  await composer.fill('A new message')
  await page.getByRole('button', { name: 'Send message' }).click()
  await expect(page.getByText('Message saved and queued for delivery', { exact: true })).toBeVisible()
  const sent = page.getByRole('article', { name: 'Your message' }).filter({ hasText: 'A new message' })
  await expect(sent.getByRole('img', { name: 'Sent' })).toBeVisible()
  await expect(sent.locator('.chat-delivery + .chat-message-bubble')).toHaveCount(1)
  await expect(sent.locator('.chat-message-bubble')).toHaveCSS('border-radius', '0px')

  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
})
