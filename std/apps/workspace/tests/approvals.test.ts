import { afterEach, expect, test } from 'bun:test'

import { api } from '../src/api/client'
import type { RulesByGrantee } from '../src/api/types'
import { chatApi } from '../src/chat/api'

const originalFetch = globalThis.fetch
afterEach(() => { globalThis.fetch = originalFetch })

function backend(options: { endpoints?: Array<{ method: string; path: string }>; grants?: RulesByGrantee; expired?: boolean; failPath?: string; status?: string | number; failAcceptance?: boolean; acceptanceRace?: 'active' | 'blocked' | 'replacement' } = {}) {
  const now = Math.floor(Date.now() / 1000)
  const endpoints = options.endpoints ?? [{ method: 'POST', path: '/std/message' }]
  const capability = { id: 'std.dhttp.net/message/message', version: '0.3.0', endpoints }
  const contact = {
    id: 7, name: 'alice.smith.dhttp.net', subject_id: 'a'.repeat(64), status: options.status ?? 'active', created_at: now - 60,
    expired_after: now + (options.expired ? -1 : 3600),
    requested_access: Object.fromEntries(endpoints.map(endpoint => [endpoint.path, [endpoint.method]])),
    // daccess's stored contact grants are not updated by PATCH /std/acl/allow.
    granted_access: {},
  }
  const rules: RulesByGrantee = structuredClone(options.grants ?? {})
  globalThis.fetch = (async (input, init) => {
    const url = new URL(String(input), 'http://localhost')
    const json = (value: unknown) => Response.json(value)
    if (url.pathname === '/std/profile') return json({ identity_name: 'bob.dhttp.net' })
    if (url.pathname === '/std/acl/capabilities') return json([capability])
    if (url.pathname === '/std/contacts') return json({ items: [contact], total: 1 })
    if (url.pathname === `/std/contact/${contact.name}`) {
      if (init?.method === 'PATCH') {
        if (options.failAcceptance) return new Response('Contact acceptance failed', { status: 500 })
        if (options.acceptanceRace) {
          contact.status = options.acceptanceRace === 'blocked' ? 'blocked' : 'active'
          if (options.acceptanceRace === 'replacement') {
            contact.id = 8
            contact.subject_id = 'b'.repeat(64)
          }
          return new Response('Contact changed', { status: 409 })
        }
        // Acceptance must follow successful writes of every capability operation.
        expect(endpoints.every(endpoint => rules[contact.name]?.[endpoint.path]?.allow.includes(endpoint.method))).toBe(true)
        contact.status = JSON.parse(String(init.body)).status
        return new Response(null, { status: 204 })
      }
      return json(contact)
    }
    if (url.pathname === '/std/acl/reviews') return json({ items: [], total: 0 })
    if (url.pathname === '/std/acl/allow') return json(rules)
    if (url.pathname === `/std/acl/allow/${contact.name}` && init?.method === 'PATCH') {
      const body = JSON.parse(String(init.body))
      if (body.api === options.failPath) return new Response('ACL write failed', { status: 500 })
      const methods = (rules[contact.name] ??= {})[body.api] ??= { allow: [], review: [], deny: [] }
      for (const effect of ['allow', 'review', 'deny'] as const) {
        methods[effect] = methods[effect].filter(method => method !== body.method)
      }
      methods[body.effect as 'allow' | 'deny'].push(body.method)
      return new Response(null, { status: 204 })
    }
    throw new Error(`Unexpected request: ${init?.method ?? 'GET'} ${url.pathname}`)
  }) as typeof fetch
  return { capability, contact }
}

for (const action of ['grant', 'deny'] as const) {
  test(`${action} removes the request and badge despite unchanged contact grants`, async () => {
    const { capability, contact } = backend()
    expect((await api.approvals('pending', 1)).total).toBe(1)
    expect((await api.context()).badges.incoming_contacts).toBe(1)
    if (action === 'grant') {
      await chatApi.grantCapability(contact.name, capability.id, { requestId: contact.id, version: capability.version })
    } else {
      await chatApi.denyCapability(contact.name, capability.id, contact.id, capability.version)
    }
    expect(contact.granted_access).toEqual({})
    expect((await api.approvals('pending', 1)).total).toBe(0)
    expect((await api.context()).badges.incoming_contacts).toBe(0)
  })
}

test('an ACL grant for another applicant does not resolve this request', async () => {
  backend({ grants: { 'carol.dhttp.net': { '/std/message': { allow: ['POST'], review: [], deny: [] } } } })
  expect((await api.approvals('pending', 1)).total).toBe(1)
})

test('a review rule leaves the capability request pending', async () => {
  backend({ grants: { 'alice.smith.dhttp.net': { '/std/message': { allow: [], review: ['POST'], deny: [] } } } })
  expect((await api.approvals('pending', 1)).total).toBe(1)
})

test('a partially failed multi-operation grant leaves the request pending', async () => {
  const { capability, contact } = backend({ endpoints: [
    { method: 'POST', path: '/std/message' }, { method: 'GET', path: '/std/message/history' },
  ], failPath: '/std/message/history' })
  await expect(chatApi.grantCapability(contact.name, capability.id)).rejects.toThrow('ACL write failed')
  expect((await api.approvals('pending', 1)).total).toBe(1)
})

test('resolved expired requests do not appear in the expired queue', async () => {
  backend({ expired: true, grants: { 'alice.smith.dhttp.net': { '/std/message': { allow: ['POST'], review: [], deny: [] } } } })
  expect((await api.approvals('expired', 1)).total).toBe(0)
})

for (const status of ['pending', 'transfered', 0, 3]) {
  test(`granting a capability accepts a ${status} contact before clearing the queue`, async () => {
    const { capability, contact } = backend({ status })
    await chatApi.grantCapability(contact.name, capability.id, { requestId: contact.id, version: capability.version })
    expect(contact.status).toBe('active')
    expect((await api.approvals('pending', 1)).total).toBe(0)
    expect((await api.context()).badges.incoming_contacts).toBe(0)
  })
}

test('an existing allow rule does not hide an unaccepted contact request', async () => {
  backend({ status: 'pending', grants: { 'alice.smith.dhttp.net': { '/std/message': { allow: ['POST'], review: [], deny: [] } } } })
  expect((await api.approvals('pending', 1)).total).toBe(1)
  expect((await api.context()).badges.incoming_contacts).toBe(1)
})

test('a denied capability does not accept the contact', async () => {
  const { capability, contact } = backend({ status: 'pending' })
  await chatApi.denyCapability(contact.name, capability.id, contact.id, capability.version)
  expect(contact.status).toBe('pending')
  expect((await api.approvals('pending', 1)).total).toBe(0)
})

test('a partially failed grant does not accept the contact', async () => {
  const { capability, contact } = backend({ status: 'pending', endpoints: [
    { method: 'POST', path: '/std/message' }, { method: 'GET', path: '/std/message/history' },
  ], failPath: '/std/message/history' })
  await expect(chatApi.grantCapability(contact.name, capability.id)).rejects.toThrow('ACL write failed')
  expect(contact.status).toBe('pending')
  expect((await api.approvals('pending', 1)).total).toBe(1)
})

test('a failed contact acceptance leaves the granted request visible for retry', async () => {
  const { capability, contact } = backend({ status: 'pending', failAcceptance: true })
  await expect(chatApi.grantCapability(contact.name, capability.id)).rejects.toThrow('Contact acceptance failed')
  expect(contact.status).toBe('pending')
  expect((await api.approvals('pending', 1)).total).toBe(1)
})

test('crossed-request reconciliation during a grant counts the same active contact as accepted', async () => {
  const { capability, contact } = backend({ status: 'pending', acceptanceRace: 'active' })
  await chatApi.grantCapability(contact.name, capability.id, { requestId: contact.id, version: capability.version })
  expect(contact.status).toBe('active')
  expect((await api.approvals('pending', 1)).total).toBe(0)
})

for (const acceptanceRace of ['blocked', 'replacement'] as const) {
  test(`a ${acceptanceRace} contact after an acceptance conflict is still rejected`, async () => {
    const { capability, contact } = backend({ status: 'pending', acceptanceRace })
    await expect(chatApi.grantCapability(contact.name, capability.id, { requestId: contact.id, version: capability.version })).rejects.toThrow('Contact changed')
  })
}
