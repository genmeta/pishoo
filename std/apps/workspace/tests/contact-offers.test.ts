import { afterEach, expect, test } from 'bun:test'
import { api, ApiError } from '../src/api/client'

const originalFetch = globalThis.fetch
afterEach(() => { globalThis.fetch = originalFetch })

function backend(rejection?: { status: number; message: string }) {
  const calls: Array<{ path: string; method: string; body: unknown }> = []
  globalThis.fetch = (async (input, init) => {
    const path = new URL(String(input), 'http://localhost').pathname
    const method = init?.method ?? 'GET'
    const body = init?.body ? JSON.parse(String(init.body)) : null
    calls.push({ path, method, body })
    if (path !== '/std/contact/local/requests' || method !== 'POST') {
      throw new Error(`Unexpected request: ${method} ${path}`)
    }
    if (rejection) return new Response(rejection.message, { status: rejection.status })
    return Response.json({ id: 1, status: 'queued', ...body }, { status: 201 })
  }) as typeof fetch
  return calls
}

test('selected requested and offered capabilities reach the native submission together', async () => {
  const calls = backend()
  const value = {
    target_name: 'iphone.alice.dhttp.net', description: 'Hello',
    requested_capabilities: ['message'],
    offered_capabilities: ['read notes'],
  }
  expect((await api.sendContactRequest(value)).status).toBe('queued')
  expect(calls).toEqual([{ path: '/std/contact/local/requests', method: 'POST', body: value }])
})

test('an empty offer remains empty in the native submission', async () => {
  const calls = backend()
  await api.sendContactRequest({
    target_name: 'iphone.alice.dhttp.net', description: '',
    requested_capabilities: ['message'], offered_capabilities: [],
  })
  expect(calls).toHaveLength(1)
  expect(calls[0].body).toMatchObject({ offered_capabilities: [] })
})

for (const rejection of [
  { status: 409, message: 'selected capability unavailable' },
  { status: 409, message: 'local contact blocks offered capabilities' },
  { status: 500, message: 'Grant failed' },
  { status: 500, message: 'Submission failed' },
]) {
  test(`native rejection is reported without separate ACL writes: ${rejection.message}`, async () => {
    const calls = backend(rejection)
    const operation = api.sendContactRequest({
      target_name: 'iphone.alice.dhttp.net', description: '', requested_capabilities: [],
      offered_capabilities: ['message'],
    })
    await expect(operation).rejects.toBeInstanceOf(ApiError)
    await expect(operation).rejects.toMatchObject(rejection)
    expect(calls).toHaveLength(1)
    expect(calls[0].path).toBe('/std/contact/local/requests')
  })
}
