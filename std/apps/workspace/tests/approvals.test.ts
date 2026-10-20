import { afterEach, expect, test } from 'bun:test'

import { api, request, ReviewRequiredError } from '../src/api/client'
import { chatApi } from '../src/chat/api'

const originalFetch = globalThis.fetch
afterEach(() => { globalThis.fetch = originalFetch })

function backend() {
  const calls: Array<{ path: string; method: string; body: unknown }> = []
  const now = Math.floor(Date.now() / 1000)
  const contact = { id: 7, incoming: true, name: 'alice.dhttp.net', subject_id: 'a'.repeat(64),
    status: 'active', created_at: now - 60, valid_before: now + 3600,
    requested_access: { '/std/message': ['POST'] }, granted_access: {}, offers: {} }
  const reviews = [
    { kind: 'live', id: 1, visitor: contact.name, method: 'POST', api: '/std/message', reason: 'review', requested_at: now, expired_after: null },
    { kind: 'persistent', id: 2, visitor: contact.name, method: 'PUT', api: '/files', reason: 'review', requested_at: now - 1, expired_after: now + 3600 },
  ]
  globalThis.fetch = (async (input, init) => {
    const url = new URL(String(input), 'http://localhost')
    const method = init?.method ?? 'GET'
    const body = init?.body ? JSON.parse(String(init.body)) : undefined
    calls.push({ path: url.pathname, method, body })
    if (url.pathname === '/std/profile') return Response.json({ identity_name: 'bob.dhttp.net' })
    if (url.pathname === '/std/contact/list') return Response.json({ items: [contact], total: 1 })
    if (url.pathname === '/std/acl/reviews') return Response.json({ items: reviews, total: reviews.length })
    if (url.pathname === '/std/acl/review') return new Response(null, { status: 204 })
    if (url.pathname === '/std/message' && method === 'PUT') return Response.json({ index: 1, state: 'queued' }, { status: 202 })
    throw new Error(`Unexpected request ${method} ${url.pathname}`)
  }) as typeof fetch
  return { calls, contact }
}

test('requested capabilities do not create synthetic access approvals or badges', async () => {
  const { calls } = backend()
  const approvals = await api.approvals('pending', 1)
  expect(approvals.total).toBe(2)
  expect(approvals.items.every(item => item.kind === 'access')).toBe(true)
  expect((await api.context()).badges).toEqual({ incoming_contacts: 0, pending_reviews: 2 })
  expect(calls.some(call => ['/std/acl/allow', '/std/acl/capabilities'].includes(call.path))).toBe(false)
})

test('contact acceptance remains a separate relationship decision', async () => {
  const { contact } = backend()
  contact.status = 'pending'
  const approvals = await api.approvals('pending', 1)
  expect(approvals.items.filter(item => item.kind === 'contact')).toEqual([{
    kind: 'contact', request_id: 7, contact_name: contact.name, subject_id: contact.subject_id,
    requested_at: contact.created_at, expired_after: contact.valid_before,
  }])
})

test('live and persistent decisions use the same durable ACL action', async () => {
  const { calls } = backend()
  const approvals = await api.approvals('pending', 1)
  expect(approvals.items.find(item => item.kind === 'access' && item.review_kind === 'live')?.expired_after).toBeNull()
  await api.decideReview(1, 'allow', 'live')
  await api.decideReview(2, 'deny', 'persistent')
  expect(calls.filter(call => call.method === 'PATCH').map(call => call.body)).toEqual([
    { id: 1, kind: 'live', action: 'allow' },
    { id: 2, kind: 'persistent', action: 'deny' },
  ])
})

test('message submission attempts delivery without querying peer permissions', async () => {
  const { calls } = backend()
  expect((await chatApi.sendMessage('alice.dhttp.net', 'hello', 'message-1')).state).toBe('queued')
  expect(calls).toEqual([{
    path: '/std/message', method: 'PUT',
    body: { to: 'alice.dhttp.net', content: 'hello', media_type: 'text/plain', client_message_id: 'message-1' },
  }])
})

test('message listing and retry use the new resource routes', async () => {
  const { calls } = backend()
  globalThis.fetch = (async (input, init) => {
    const url = new URL(String(input), 'http://localhost')
    const method = init?.method ?? 'GET'
    calls.push({ path: `${url.pathname}${url.search}`, method, body: undefined })
    if (url.pathname === '/std/message/list') return Response.json({ items: [], next_cursor: null })
    if (url.pathname === '/std/message/retry') return Response.json({ index: 42, state: 'queued' })
    throw new Error(`Unexpected request ${method} ${url.pathname}`)
  }) as typeof fetch
  await chatApi.messages('alice.dhttp.net', '7', 100)
  await chatApi.retryMessage(42)
  expect(calls).toEqual([
    { path: '/std/message/list?name=alice.dhttp.net&after=7&limit=100', method: 'GET', body: undefined },
    { path: '/std/message/retry?index=42', method: 'PUT', body: undefined },
  ])
})

test('requests default to break and expose need_review separately from business success', async () => {
  const modes: Array<string | null> = []
  globalThis.fetch = (async (_input, init) => {
    modes.push(new Headers(init?.headers).get('if-reviewed'))
    return Response.json({ error: 'need_review', review_id: 12, status_url: '/std/acl/status/12' }, { status: 400 })
  }) as typeof fetch
  await expect(request('/private')).rejects.toBeInstanceOf(ReviewRequiredError)
  await expect(request('/private', { headers: { 'if-reviewed': 'waiting' } })).rejects.toMatchObject({ reviewId: 12, statusUrl: '/std/acl/status/12' })
  expect(modes).toEqual(['break', 'waiting'])
})
