import { expect, test } from '@playwright/test'
import type { Page } from '@playwright/test'

import type { OutboundContactInput, OutboundContactRequest } from '../src/api/types'

async function mockOutbound(page: Page): Promise<void> {
  const sent: OutboundContactRequest[] = []
  await page.route('**/workspace-api/context', (route) => route.fulfill({ json: {
    profile: 'owner.local', owner_name: 'owner.local',
    badges: { pending_reviews: 0, incoming_contacts: null },
  } }))
  await page.route('**/workspace-api/settings/profile', (route) => route.fulfill({ json: {
    identity_name: 'owner.local', display_name: null, avatar_url: null, updated_at: 1700000000,
  } }))
  await page.route('**/workspace-api/capabilities', (route) => route.fulfill({ json: [
    {
      id: 'public_profile', version: '1', visibility: 'public', approval_mode: 'none',
      selectable: false, endpoints: [{ method: 'GET', path: '/std/profile' }],
    },
    {
      id: 'chat', version: '1', visibility: 'contact', approval_mode: 'capability',
      selectable: true, endpoints: [{ method: 'POST', path: '/std/message' }],
    },
  ] }))
  await page.route('**/acl/reviews?*', (route) => route.fulfill({ json: {
    items: [], total: 0, page: 1, page_size: 20,
  } }))
  await page.route('**/contacts?*', (route) => route.fulfill({ json: {
    items: [], total: 0, page: 1, page_size: 100,
  } }))
  await page.route('**/workspace-api/contact-requests**', (route) => {
    const method = route.request().method()
    const path = new URL(route.request().url()).pathname
    const id = Number(path.match(/contact-requests\/(\d+)/)?.[1])
    if (method === 'POST' && path.endsWith('/contact-requests')) {
      const body = route.request().postDataJSON() as OutboundContactInput
      const canonicalName = body.target_name.endsWith('.dhttp.net')
        ? body.target_name : `${body.target_name}.dhttp.net`
      if (sent.some((item) => item.target_name === canonicalName && ['queued', 'pending'].includes(item.status))) {
        return route.fulfill({ status: 409, body: 'pending request already exists' })
      }
      const created: OutboundContactRequest = {
        ...body, target_name: canonicalName, id: sent.length + 1, status: 'queued',
        expired_after: 1700000000 + 7 * 86400,
        delivery_deadline: 1700000000 + 7 * 86400, remote_expired_after: null,
        last_checked_at: null, error_message: null,
        created_at: 1700000000, updated_at: 1700000000,
      }
      sent.unshift(created)
      return route.fulfill({ status: 202, json: created })
    }
    if (method === 'POST' && path.endsWith('/refresh')) {
      const item = sent.find((entry) => entry.id === id)!
      item.status = 'active'
      item.last_checked_at = 1700000100
      return route.fulfill({ json: item })
    }
    if (method === 'DELETE') {
      sent.splice(sent.findIndex((item) => item.id === id), 1)
      return route.fulfill({ status: 204, body: '' })
    }
    if (id) return route.fulfill({ json: sent.find((item) => item.id === id) })
    return route.fulfill({ json: { items: sent, total: sent.length, page: 1, page_size: 20 } })
  })
}

test.beforeEach(async ({ page }) => mockOutbound(page))

test('select chat access, check status and delete local record', async ({ page }, testInfo) => {
  await page.goto('./contacts/new')
  await expect(page.getByRole('heading', { name: 'Add contact' })).toBeVisible()
  await page.getByRole('textbox', { name: 'Recipient DHTTP name' }).fill('friend.example')
  await page.getByRole('textbox', { name: 'Introduction' }).fill('Hello there')
  await expect(page.getByRole('group', { name: 'Capabilities to request from the other side', exact: true })).toBeVisible()
  await expect(page.getByRole('group', { name: 'Capabilities to offer to the other side', exact: true })).toBeVisible()
  const chat = page.getByRole('checkbox', { name: /One-to-one chat/ }).first()
  await expect(chat).not.toBeChecked()
  await chat.check()
  await page.screenshot({ path: testInfo.outputPath('new-contact.png'), fullPage: true })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true)
  await page.getByRole('button', { name: 'Send request' }).click()
  await expect(page).toHaveURL(/\/workspace\/contacts\/requests$/)
  await expect(page.getByRole('heading', { name: 'Sent requests' })).toBeVisible()
  await expect(page.getByText('friend.example', { exact: true })).toBeVisible()
  await expect(page.locator('tbody')).not.toContainText('friend.example.dhttp.net')
  await page.reload()
  await expect(page.getByRole('heading', { name: 'Sent requests' })).toBeVisible()
  await page.goBack()
  await page.goForward()
  await expect(page.getByRole('heading', { name: 'Sent requests' })).toBeVisible()
  await page.getByRole('button', { name: 'Check status' }).click()
  await expect(page.getByText('Active', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Delete', exact: true }).click()
  const confirmation = page.getByRole('dialog', { name: 'Delete sent request' })
  await expect(confirmation).toContainText('friend.example')
  await expect(confirmation).not.toContainText('friend.example.dhttp.net')
  await expect(confirmation).toContainText('will not withdraw')
  await confirmation.getByRole('button', { name: 'Delete' }).click()
  await expect(page.getByText('No sent requests')).toBeVisible()
})

test('offline requests queue locally and duplicates stay editable', async ({ page }) => {
  await page.goto('./contacts/new')
  await page.getByRole('checkbox', { name: /One-to-one chat/ }).first().check()
  await page.getByRole('textbox', { name: 'Recipient DHTTP name' }).fill('offline.example')
  await page.getByRole('textbox', { name: 'Introduction' }).fill('Keep this text')
  await page.getByRole('button', { name: 'Send request' }).click()
  await expect(page.getByText('offline.example', { exact: true })).toBeVisible()
  await expect(page.getByText('Queued', { exact: true })).toBeVisible()
  await page.goto('./contacts/new')
  await page.getByRole('textbox', { name: 'Recipient DHTTP name' }).fill('offline.example.dhttp.net')
  await page.getByRole('checkbox', { name: /One-to-one chat/ }).first().check()
  await page.getByRole('button', { name: 'Send request' }).click()
  await expect(page.getByText('A pending request to this name already exists.')).toBeVisible()
})

test('can create a contact without requesting an optional capability', async ({ page }) => {
  await page.goto('./contacts/new')
  await page.getByRole('textbox', { name: 'Recipient DHTTP name' }).fill('profile-only.example')
  await page.getByRole('textbox', { name: 'Introduction' }).fill('Profile only')
  await expect(page.getByRole('checkbox', { name: /One-to-one chat/ }).first()).not.toBeChecked()
  await page.getByRole('button', { name: 'Send request' }).click()
  await expect(page).toHaveURL(/\/workspace\/contacts\/requests$/)
  await expect(page.getByText('profile-only.example', { exact: true })).toBeVisible()
})

test('capability option text follows the selected locale', async ({ page }) => {
  await page.goto('./contacts/new')
  await expect(page.getByRole('checkbox', { name: /One-to-one chat/ }).first()).toBeVisible()
  await page.getByRole('button', { name: 'Chinese' }).click()
  await expect(page.getByRole('checkbox', { name: /一对一聊天/ }).first()).toBeVisible()
  await expect(page.getByText('向对方发送消息。', { exact: true })).toBeVisible()
  await expect(page.getByText('允许对方向我发送消息。', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: '英文' }).click()
  await expect(page.getByRole('checkbox', { name: /One-to-one chat/ }).first()).toBeVisible()
  await expect(page.getByText('Send messages to the other side.', { exact: true })).toBeVisible()
})
