import { expect, test } from '@playwright/test'

for (const kind of ['live', 'persistent'] as const) {
  test(`${kind} approval saves an ACL decision without a one-request expiry`, async ({ page }) => {
    const decisions: unknown[] = []
    const time = Math.floor(Date.now() / 1000)
    const review = { kind, id: 12, visitor: 'alice.dhttp.net', method: 'POST', api: '/std/message',
      reason: 'review', requested_at: time, expired_after: kind === 'live' ? null : time + 3600 }
    await page.route('**/std/**', route => {
      const url = new URL(route.request().url())
      if (url.pathname === '/std/profile') return route.fulfill({ json: { identity_name: 'owner.dhttp.net', avatar_url: null, updated_at: time } })
      if (url.pathname === '/std/contact/list') return route.fulfill({ json: { items: [], total: 0 } })
      if (url.pathname === '/std/acl/capabilities') return route.fulfill({ json: [] })
      if (url.pathname === '/std/acl/reviews') return route.fulfill({ json: { items: decisions.length ? [] : [review], total: decisions.length ? 0 : 1 } })
      if (url.pathname === '/std/acl/review') {
        decisions.push(route.request().postDataJSON())
        return route.fulfill({ status: 204 })
      }
      return route.fulfill({ status: 404 })
    })
    await page.goto('/#/approvals')
    const row = page.locator('tbody tr').filter({ hasText: 'POST /std/message' })
    await row.getByRole('button', { name: 'Allow', exact: true }).click()
    const dialog = page.getByRole('dialog', { name: /Allow request/ })
    await expect(dialog).toContainText('access rule')
    await expect(dialog.locator('input[type="datetime-local"]')).toHaveCount(0)
    await dialog.getByRole('button', { name: 'Allow', exact: true }).click()
    await expect(page.getByText('Request allowed', { exact: true })).toBeVisible()
    expect(decisions).toEqual([{ id: 12, kind, action: 'allow' }])
    await expect(page.locator('tbody tr')).toHaveCount(0)
  })
}

test('accepting a contact only changes the relationship', async ({ page }) => {
  const mutations: string[] = []
  const time = Math.floor(Date.now() / 1000)
  const contact = { id: 7, incoming: true, name: 'alice.dhttp.net', subject_id: 'a'.repeat(64), status: 'pending',
    alias: null, class: 'Human', description: '', created_at: time, updated_at: time, valid_before: time + 3600,
    requested_access: { '/std/message': ['POST'] }, granted_access: {}, offers: {} }
  await page.route('**/std/**', route => {
    const request = route.request()
    const url = new URL(request.url())
    if (request.method() !== 'GET') mutations.push(`${request.method()} ${url.pathname}`)
    if (url.pathname === '/std/profile') return route.fulfill({ json: { identity_name: 'owner.dhttp.net', updated_at: time } })
    if (url.pathname === '/std/contact/list') return route.fulfill({ json: { items: [contact], total: 1 } })
    if (url.pathname === '/std/acl/reviews') return route.fulfill({ json: { items: [], total: 0 } })
    if (url.pathname === '/std/acl/capabilities') return route.fulfill({ json: [] })
    if (url.pathname === '/std/contact') {
      if (request.method() === 'PATCH') {
        Object.assign(contact, request.postDataJSON())
        return route.fulfill({ status: 204 })
      }
      return route.fulfill({ json: contact })
    }
    return route.fulfill({ status: 404 })
  })
  await page.goto('/#/approvals')
  await page.getByRole('button', { name: 'Accept contact', exact: true }).click()
  await expect(page.locator('tbody tr')).toHaveCount(0)
  expect(mutations).toEqual(['PATCH /std/contact'])
})

for (const populated of [false, true]) {
  test(`approval polling preserves the ${populated ? 'table and focus' : 'empty state'} while loading and after failures`, async ({ page }) => {
    const now = new Date()
    const time = Math.floor(now.getTime() / 1000)
    await page.clock.install({ time: now })
    await page.clock.pauseAt(now)
    const review = { kind: 'live', id: 12, visitor: 'alice.dhttp.net', method: 'POST', api: '/std/message',
      reason: 'review', requested_at: time, expired_after: null as number | null }
    let reviews = populated ? [review, { ...review, kind: 'persistent', expired_after: time + 3600 }] : []
    let releasePoll!: () => void
    const pollGate = new Promise<void>(resolve => { releasePoll = resolve })
    let holdPoll = false
    let failPoll = false
    let polls = 0
    await page.route('**/std/**', async route => {
      const url = new URL(route.request().url())
      const json = (value: unknown) => route.fulfill({ json: value })
      if (url.pathname === '/std/profile') return json({ identity_name: 'owner.dhttp.net', updated_at: time })
      if (url.pathname === '/std/contact/list') return json({ items: [], total: 0 })
      if (url.pathname === '/std/acl/capabilities') return json([])
      if (url.pathname === '/std/acl/reviews') {
        if (url.searchParams.has('status')) {
          polls++
          if (holdPoll) await pollGate
          if (failPoll) return route.fulfill({ status: 503, body: 'Temporarily unavailable' })
        }
        return json({ items: reviews, total: reviews.length })
      }
      return route.fulfill({ status: 404 })
    })

    try {
      await page.goto('/#/approvals')
      await expect(page.locator('.toolbar-meta')).toHaveText(`${reviews.length} pending`)
      const rows = await page.locator('tbody tr').elementHandles()
      const empty = populated ? null : await page.locator('.state-empty').elementHandle()
      const allow = page.locator('tbody tr').first().getByRole('button', { name: 'Allow', exact: true })
      if (populated) await allow.focus()
      const expectStable = async () => {
        await expect(page.locator('.spinner')).toHaveCount(0)
        await expect(page.locator('.toolbar-meta')).toHaveText(`${populated ? 2 : 0} pending`)
        await expect(page.getByRole('button', { name: 'Refresh', exact: true })).toBeEnabled()
        for (const [index, row] of rows.entries()) {
          expect(await row.evaluate((node, index) => node === document.querySelectorAll('tbody tr')[index], index)).toBe(true)
        }
        if (populated) await expect(allow).toBeFocused()
        else expect(await empty!.evaluate(node => node === document.querySelector('.state-empty'))).toBe(true)
      }
      const nextPollResponse = () => page.waitForResponse(response => {
        const url = new URL(response.url())
        return url.pathname === '/std/acl/reviews' && url.searchParams.has('status')
      })

      holdPoll = true
      const unchangedResponse = nextPollResponse()
      await page.clock.runFor(3000)
      await expect.poll(() => polls).toBe(2)
      await expectStable()
      releasePoll()
      await unchangedResponse
      await page.clock.runFor(20)
      await expectStable()

      holdPoll = false
      failPoll = true
      const failedResponse = nextPollResponse()
      await page.clock.runFor(3000)
      await failedResponse
      await page.clock.runFor(20)
      await expectStable()
      await expect(page.locator('.state-error')).toHaveCount(0)

      failPoll = false
      reviews = [...reviews, { ...review, id: 13, visitor: 'bob.dhttp.net', requested_at: time - 1 }]
      await page.clock.runFor(3000)
      await expect(page.locator('tbody tr')).toHaveCount(populated ? 3 : 1)
      await expect(page.locator('.toolbar-meta')).toHaveText(`${reviews.length} pending`)
      for (const row of rows) expect(await row.evaluate(node => node.isConnected)).toBe(true)
      if (populated) await expect(allow).toBeFocused()
    } finally {
      releasePoll()
    }
  })
}

test('a background approval response cannot replace a newly selected tab', async ({ page }) => {
  const now = new Date()
  const time = Math.floor(now.getTime() / 1000)
  await page.clock.install({ time: now })
  await page.clock.pauseAt(now)
  let releasePoll!: () => void
  const pollGate = new Promise<void>(resolve => { releasePoll = resolve })
  let holdPoll = false
  await page.route('**/std/**', async route => {
    const url = new URL(route.request().url())
    const json = (value: unknown) => route.fulfill({ json: value })
    if (url.pathname === '/std/profile') return json({ identity_name: 'owner.dhttp.net', updated_at: time })
    if (url.pathname === '/std/contact/list') return json({ items: [], total: 0 })
    if (url.pathname === '/std/acl/capabilities') return json([])
    if (url.pathname === '/std/acl/reviews') {
      if (url.searchParams.get('status') === 'pending' && holdPoll) await pollGate
      const expired = url.searchParams.get('status') === 'expired'
      return json({ items: [{ kind: 'persistent', id: expired ? 2 : 1,
        visitor: expired ? 'expired.dhttp.net' : 'pending.dhttp.net', method: 'GET', api: '/files',
        reason: 'review', requested_at: time, expired_after: expired ? time - 1 : time + 3600 }], total: 1 })
    }
    return route.fulfill({ status: 404 })
  })

  try {
    await page.goto('/#/approvals')
    await expect(page.locator('tbody tr')).toContainText('pending')
    holdPoll = true
    const pendingResponse = page.waitForResponse(response => new URL(response.url()).searchParams.get('status') === 'pending')
    await page.clock.runFor(3000)
    await page.getByRole('button', { name: 'Expired', exact: true }).click()
    await expect(page.locator('tbody tr')).toContainText('expired')
    releasePoll()
    await pendingResponse
    await page.clock.runFor(20)
    await expect(page.locator('tbody tr')).toContainText('expired')
    await expect(page.locator('tbody tr')).not.toContainText('pending')
  } finally {
    releasePoll()
  }
})
