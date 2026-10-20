import { expect, test } from '@playwright/test'
import type { Page } from '@playwright/test'

import type { Contact } from '../src/api/types'

function contact(name: string, status: Contact['status'] = 'active', alias: string | null = null): Contact {
  return { id: 1, incoming: false, name, status, alias, subject_id: 'a'.repeat(64), class: 'Human',
    description: '', created_at: 1700000000, updated_at: 1700000000, valid_before: 2000000000,
    requested_access: {}, granted_access: {}, offers: {} }
}

async function mockDirectory(page: Page, contacts: Contact[], profileGate?: Promise<void>): Promise<string[]> {
  const requests: string[] = []
  await page.route('**/std/**', async route => {
    const request = route.request()
    const url = new URL(request.url())
    requests.push(`${request.method()} ${url.pathname}${url.search}`)
    const json = (value: unknown) => route.fulfill({ json: value })
    if (url.pathname === '/std/profile') return json({ identity_name: 'owner.dhttp.net', display_name: 'Owner', avatar_url: null, updated_at: 1 })
    if (url.pathname === '/std/acl/reviews') return json({ items: [], total: 0, page: 1, page_size: 20 })
    if (url.pathname === '/std/acl/capabilities' || url.pathname.endsWith('/std/acl/capabilities')) return json([])
    if (url.pathname === '/std/acl/allow') return json({})
    if (url.pathname === '/std/contact/list') {
      const currentPage = Number(url.searchParams.get('page') ?? 1)
      const pageSize = Number(url.searchParams.get('page_size') ?? 20)
      return json({ items: contacts.slice((currentPage - 1) * pageSize, currentPage * pageSize), total: contacts.length, page: currentPage, page_size: pageSize })
    }
    if (url.pathname === '/std/contact') {
      const item = contacts.find(contact => contact.name === url.searchParams.get('name'))
      if (!item) return route.fulfill({ status: 404 })
      if (request.method() === 'PATCH') {
        Object.assign(item, request.postDataJSON())
        return route.fulfill({ status: 204 })
      }
      return json(item)
    }
    if (url.pathname.endsWith('/std/profile')) {
      await profileGate
      const name = url.pathname.split('/')[3]
      return json({ display_name: name === 'bob.dhttp.net' ? 'Alpha' : 'Beta', avatar_url: '/std/profile/avatar', updated_at: 42 })
    }
    if (url.pathname.endsWith('/profile/avatar')) return route.fulfill({ contentType: 'image/png',
      body: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=', 'base64') })
    return route.fulfill({ status: 404, body: `Unmocked ${url.pathname}` })
  })
  return requests
}

test('contact cards render and stay interactive while remote profiles are pending', async ({ page }) => {
  let releaseProfiles!: () => void
  const profileGate = new Promise<void>(resolve => { releaseProfiles = resolve })
  const requests = await mockDirectory(page, [contact('bob.dhttp.net')], profileGate)
  const profileRequest = page.waitForRequest(request =>
    request.url().includes('/std/dhttp/bob.dhttp.net/std/profile'))

  try {
    await page.goto('/#/contacts')
    await profileRequest
    const card = page.locator('.contact-card')
    await expect(card.locator('.contact-link strong')).toHaveText('bob')
    await expect(card.getByRole('button', { name: 'Open bob', exact: true })).toBeVisible()
    await page.getByRole('searchbox').fill('bob')
    await expect(card).toBeVisible()

    releaseProfiles()
    await expect(card.locator('.contact-link strong')).toHaveText('Alpha')
    expect(requests.filter(request => request.startsWith('GET /std/dhttp/bob.dhttp.net/std/profile'))).toHaveLength(1)
  } finally {
    releaseProfiles()
  }
})

test('contact cards include pending and contacts without chat, hide blocked, and use nickname order', async ({ page }) => {
  const requests = await mockDirectory(page, [contact('alice.dhttp.net', 'active', 'Zulu'),
    contact('bob.dhttp.net', 'pending'), contact('carol.dhttp.net'), contact('blocked.dhttp.net', 'blocked')])
  await page.goto('/#/contacts')
  await expect(page.locator('.contact-card .contact-link strong')).toHaveText(['Alpha', 'Beta', 'Zulu'])
  await expect(page.locator('.contact-card')).toHaveCount(3)
  await expect(page.locator('.contact-card').first()).toContainText('Pending')
  await expect(page.getByRole('button', { name: /^Chat with / })).toHaveCount(3)
  await expect(page.locator('main .section-nav')).toHaveCount(0)
  await expect(page.locator('.contact-card img').first()).toHaveAttribute('src', /\/std\/contact\/bob\.dhttp\.net\/profile\/avatar\?v=42/)
  expect(requests.some(request => request.includes('/local/') || request.startsWith('POST '))).toBe(false)
  await page.screenshot({ path: test.info().outputPath('contacts.png'), fullPage: true })
  await page.getByRole('searchbox').fill('Beta')
  await expect(page.locator('.contact-card')).toHaveCount(1)
  await page.setViewportSize({ width: 375, height: 812 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
})

test('editing an alias reorders cards before pagination', async ({ page }) => {
  const contacts = Array.from({ length: 22 }, (_, index) => contact(`person-${index}.dhttp.net`, 'active', `Person ${String(index).padStart(2, '0')}`))
  contacts[21].alias = 'Alpha'
  await mockDirectory(page, contacts)
  await page.goto('/#/contacts')
  await expect(page.locator('.contact-card')).toHaveCount(20)
  await expect(page.locator('.contact-card .contact-link strong').first()).toHaveText('Alpha')
  await page.locator('.contact-card .contact-link').nth(1).click()
  const drawer = page.locator('.detail-drawer')
  await drawer.getByRole('textbox', { name: 'Local alias' }).fill('Aardvark')
  await drawer.getByRole('button', { name: 'Save alias', exact: true }).click()
  await expect(page.locator('.contact-card .contact-link strong').first()).toHaveText('Aardvark')
})

test('opening and closing add contact preserves cards, search, pagination, and directory requests', async ({ page }) => {
  const contacts = Array.from({ length: 22 }, (_, index) =>
    contact(`person-${index}.dhttp.net`, 'active', `Person ${String(index).padStart(2, '0')}`))
  const requests = await mockDirectory(page, contacts)
  await page.goto('/#/contacts')
  await page.waitForLoadState('networkidle')
  await page.getByRole('searchbox').fill('Person')
  await page.getByRole('button', { name: 'Next page' }).click()
  await expect(page.locator('.contact-card')).toHaveCount(2)
  const card = await page.locator('.contact-card').first().elementHandle()
  const initialRequests = [...requests]
  const dialog = page.getByRole('dialog', { name: 'Find contact' })

  await page.getByRole('button', { name: 'Add contact', exact: true }).click()
  await expect(dialog).toBeVisible()
  expect(await card!.evaluate(node => node === document.querySelector('.contact-card'))).toBe(true)
  await dialog.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(dialog).toHaveCount(0)
  await expect(page.getByRole('searchbox')).toHaveValue('Person')
  await expect(page.locator('.contact-card')).toHaveCount(2)

  await page.getByRole('button', { name: 'Add contact', exact: true }).click()
  await expect(dialog).toBeVisible()
  await page.goBack()
  await expect(dialog).toHaveCount(0)
  await page.goForward()
  await expect(dialog).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(dialog).toHaveCount(0)
  expect(await card!.evaluate(node => node === document.querySelector('.contact-card'))).toBe(true)
  await expect(page.getByRole('searchbox')).toHaveValue('Person')
  await expect(page.locator('.contact-card')).toHaveCount(2)
  expect(requests).toEqual(initialRequests)
})

test('changing the recipient and canceling restores the same directory cards', async ({ page }) => {
  const requests = await mockDirectory(page, [contact('bob.dhttp.net')])
  await page.goto('/#/contacts')
  await page.waitForLoadState('networkidle')
  const card = await page.locator('.contact-card').elementHandle()
  const directoryRequests = () => requests.filter(request =>
    request.startsWith('GET /std/contact/list') || request.includes('bob.dhttp.net'))
  const initialRequests = directoryRequests()

  await page.getByRole('button', { name: 'Add contact', exact: true }).click()
  await page.getByRole('textbox', { name: 'Recipient DHTTP name' }).fill('friend')
  await page.getByRole('button', { name: 'Search', exact: true }).click()
  await expect(page.getByRole('textbox', { name: 'Introduction', exact: true })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Contacts', exact: true })).toHaveCount(0)
  await expect(page.locator('.contact-card')).toBeHidden()
  expect(await card!.evaluate(node => node.isConnected)).toBe(true)

  await page.getByRole('button', { name: 'Change contact', exact: true }).click()
  const dialog = page.getByRole('dialog', { name: 'Find contact' })
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(page.locator('.contact-card')).toBeVisible()
  expect(await card!.evaluate(node => node === document.querySelector('.contact-card'))).toBe(true)
  expect(directoryRequests()).toEqual(initialRequests)
})

test('contact detail stops polling and chat back returns to the directory', async ({ page }) => {
  const requests = await mockDirectory(page, [contact('bob.dhttp.net')])
  await page.goto('/#/contacts/bob.dhttp.net')
  await expect(page.getByRole('complementary', { name: 'Contact details' })).toBeVisible()
  await page.waitForTimeout(3500)

  expect(requests.filter(request => request.startsWith('GET /std/contact?name=bob.dhttp.net'))).toHaveLength(1)
  expect(requests.filter(request => request.startsWith('GET /std/dhttp/bob.dhttp.net/std/profile'))).toHaveLength(1)

  await page.goto('/#/contacts')
  await page.getByRole('button', { name: /^Chat with / }).click()
  await expect(page).toHaveURL(/\/\#\/contacts\/bob\.dhttp\.net\/chat$/)
  await page.getByRole('button', { name: 'Back to contacts' }).click()
  await expect(page).toHaveURL(/\/\#\/contacts$/)
  await expect(page.getByRole('complementary', { name: 'Contact details' })).toHaveCount(0)
})
