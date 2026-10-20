import { expect, test } from '@playwright/test'
import type { Page } from '@playwright/test'

import type { OutboundContactInput, OutboundContactRequest } from '../src/api/types'

async function mockOutbound(page: Page) {
  const sent: OutboundContactRequest[] = []
  const calls: string[] = []
  await page.route('**/std/**', route => {
    const request = route.request()
    const url = new URL(request.url())
    calls.push(`${request.method()} ${url.pathname}`)
    const json = (value: unknown) => route.fulfill({ json: value })
    if (url.pathname === '/std/profile') return json({ identity_name: 'owner.dhttp.net', display_name: 'Owner', avatar_url: null, updated_at: 1 })
    if (url.pathname.endsWith('/std/profile')) return route.fulfill({ status: 502, body: 'Peer offline' })
    if (url.pathname === '/std/contact/list' || url.pathname === '/std/acl/reviews') return json({ items: [], total: 0, page: 1, page_size: 100 })
    if (url.pathname === '/std/contact' && request.method() === 'POST') {
      const body = request.postDataJSON() as OutboundContactInput
      if (sent.some(item => item.target_name === body.target_name)) return route.fulfill({ status: 409, body: 'pending request already exists' })
      const created: OutboundContactRequest = { ...body, id: sent.length + 1, status: 'queued',
        valid_before: 2000000000, delivery_deadline: 2000000000, remote_valid_before: null,
        last_checked_at: null, error_message: null, created_at: 1700000000, updated_at: 1700000000 }
      sent.unshift(created)
      return route.fulfill({ status: 202, json: created })
    }
    if (url.pathname === '/std/contact/requests/refresh') {
      const item = sent.find(entry => entry.id === Number(url.searchParams.get('id')))!
      item.status = 'active'
      item.last_checked_at = 1700000100
      return json(item)
    }
    if (url.pathname === '/std/contact/requests') {
      if (request.method() === 'DELETE') {
        sent.splice(sent.findIndex(item => item.id === Number(url.searchParams.get('id'))), 1)
        return route.fulfill({ status: 204 })
      }
      return json({ items: sent, total: sent.length, page: 1, page_size: 20 })
    }
    return route.fulfill({ status: 404, body: `Unmocked ${url.pathname}` })
  })
  return { sent, calls }
}

async function findRecipient(page: Page, name: string) {
  await page.goto('/#/contacts/new')
  await page.getByRole('textbox', { name: 'Recipient DHTTP name' }).fill(name)
  await page.getByRole('button', { name: 'Search', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Add contact' })).toBeVisible()
}

test('contact requests work offline without capability applications and preserve status management', async ({ page }) => {
  const { sent, calls } = await mockOutbound(page)
  await findRecipient(page, 'friend')
  await expect(page.getByRole('checkbox')).toHaveCount(0)
  await page.getByRole('textbox', { name: 'Introduction', exact: true }).fill('Hello there')
  await page.getByRole('button', { name: 'Send request' }).click()
  await expect(page.getByRole('heading', { name: 'Sent requests' })).toBeVisible()
  await expect(page.getByText('friend', { exact: true })).toBeVisible()
  expect(sent[0]).toMatchObject({ target_name: 'friend.dhttp.net', requested_capabilities: [], offered_capabilities: [] })
  expect(calls.some(path => path.includes('capability') || path.includes('capabilities'))).toBe(false)
  await page.getByRole('button', { name: 'Check status' }).click()
  await expect(page.getByText('Active', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Delete', exact: true }).click()
  const confirmation = page.getByRole('dialog', { name: 'Delete sent request' })
  await expect(confirmation).toContainText('will not withdraw')
  await confirmation.getByRole('button', { name: 'Delete' }).click()
  await expect(page.getByText('No sent requests')).toBeVisible()
})

test('duplicate contact requests remain editable', async ({ page }) => {
  await mockOutbound(page)
  await findRecipient(page, 'offline')
  await page.getByRole('button', { name: 'Send request' }).click()
  await expect(page.getByText('Queued', { exact: true })).toBeVisible()
  await findRecipient(page, 'offline.dhttp.net')
  await page.getByRole('textbox', { name: 'Introduction', exact: true }).fill('Keep this text')
  await page.getByRole('button', { name: 'Send request' }).click()
  await expect(page.getByText('A pending request to this name already exists.')).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Introduction', exact: true })).toHaveValue('Keep this text')
})
