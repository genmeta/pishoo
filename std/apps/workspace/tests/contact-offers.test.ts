import { afterEach, expect, test } from 'bun:test'
import { api } from '../src/api/client'

const originalFetch = globalThis.fetch
afterEach(() => { globalThis.fetch = originalFetch })

function backend(options: { failGrant?: boolean; failSubmit?: boolean } = {}) {
  const calls: Array<{ path: string; method: string; body: any }> = []
  const grants: any[] = []
  const catalog = [
    { id: 'alice.dhttp.net/message#message', selectable: true, endpoints: [{ method: 'POST', path: '/std/message' }] },
    { id: 'alice.dhttp.net/note#read', selectable: true, endpoints: [{ method: 'GET', path: '/std/note' }, { method: 'GET', path: '/std/note/items' }] },
  ]
  globalThis.fetch = (async (input, init) => {
    const path = new URL(String(input), 'http://localhost').pathname
    const method = init?.method ?? 'GET'
    const body = init?.body ? JSON.parse(String(init.body)) : null
    calls.push({ path, method, body })
    if (path === '/std/acl/capabilities') return Response.json(catalog)
    if (path === '/std/acl/allow/iphone.alice.dhttp.net' && method === 'PATCH') {
      if (options.failGrant) return new Response('Grant failed', { status: 500 })
      grants.push(body)
      return new Response(null, { status: 204 })
    }
    if (path === '/std/contact/local/requests' && method === 'POST') {
      if (options.failSubmit) return new Response('Submission failed', { status: 500 })
      return Response.json({ id: 1, status: 'queued', ...body }, { status: 201 })
    }
    throw new Error(`Unexpected request: ${method} ${path}`)
  }) as typeof fetch
  return { calls, grants, catalog }
}

test('submitting selected offers grants the peer before sending the friend request', async () => {
  const { calls, grants, catalog } = backend()
  const value = {
    target_name: 'iphone.alice.dhttp.net', description: 'Hello',
    requested_capabilities: ['iphone.alice.dhttp.net/message#message'], offered_capabilities: [catalog[0].id],
  }
  expect((await api.sendContactRequest(value)).status).toBe('queued')
  expect(grants).toEqual([{ api: '/std/message', method: 'POST', effect: 'allow' }])
  expect(calls.map(call => [call.method, call.path])).toEqual([
    ['GET', '/std/acl/capabilities'], ['PATCH', '/std/acl/allow/iphone.alice.dhttp.net'], ['POST', '/std/contact/local/requests'],
  ])
  expect(calls.at(-1)?.body).toEqual(value)
})

test('only the explicitly offered operations are granted', async () => {
  const { grants, catalog } = backend()
  await api.sendContactRequest({ target_name: 'iphone.alice.dhttp.net', description: '', requested_capabilities: [catalog[0].id], offered_capabilities: [catalog[1].id] })
  expect(grants).toEqual([
    { api: '/std/note', method: 'GET', effect: 'allow' }, { api: '/std/note/items', method: 'GET', effect: 'allow' },
  ])
})

test('an unselected offer grants nothing', async () => {
  const { calls, grants } = backend()
  await api.sendContactRequest({ target_name: 'iphone.alice.dhttp.net', description: '', requested_capabilities: [], offered_capabilities: [] })
  expect(grants).toEqual([])
  expect(calls.map(call => call.path)).toEqual(['/std/contact/local/requests'])
})

test('unknown offers are rejected before any ACL or request writes', async () => {
  const { calls, grants, catalog } = backend()
  await expect(api.sendContactRequest({ target_name: 'iphone.alice.dhttp.net', description: '', requested_capabilities: [], offered_capabilities: [catalog[0].id, 'missing'] })).rejects.toThrow()
  expect(grants).toEqual([])
  expect(calls.every(call => call.method === 'GET')).toBe(true)
})

test('a failed grant does not submit an offer that is not actually authorized', async () => {
  const { calls, catalog } = backend({ failGrant: true })
  await expect(api.sendContactRequest({ target_name: 'iphone.alice.dhttp.net', description: '', requested_capabilities: [], offered_capabilities: [catalog[0].id] })).rejects.toThrow('Grant failed')
  expect(calls.some(call => call.path === '/std/contact/local/requests')).toBe(false)
})

test('a failed submission is reported after the explicit permission grant', async () => {
  const { grants, catalog } = backend({ failSubmit: true })
  await expect(api.sendContactRequest({ target_name: 'iphone.alice.dhttp.net', description: '', requested_capabilities: [], offered_capabilities: [catalog[0].id] })).rejects.toThrow('Submission failed')
  expect(grants).toEqual([{ api: '/std/message', method: 'POST', effect: 'allow' }])
})
