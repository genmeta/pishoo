import { expect, test } from 'bun:test'

import { api, ApiError } from '../src/api/client'

test('offering capabilities submits one native request without granting an unknown contact from the browser', async () => {
  const originalFetch = globalThis.fetch
  const calls: string[] = []
  const input = {
    target_name: 'sky.lee~', description: 'Hello',
    requested_capabilities: [], offered_capabilities: ['dev.dhttp.net/message#message'],
  }
  globalThis.fetch = (async (url, init) => {
    calls.push(String(url))
    if (url !== '/std/contact') {
      return new Response('contact does not exist', { status: 400 })
    }
    expect(init?.method).toBe('POST')
    expect(JSON.parse(String(init?.body))).toEqual({ ...input, target_name: 'sky.lee.dhttp.net' })
    return Response.json({ id: 1, status: 'queued' }, { status: 201 })
  }) as typeof fetch
  try {
    expect(await api.sendContactRequest(input)).toMatchObject({ id: 1, status: 'queued' })
    expect(calls).toEqual(['/std/contact'])
  } finally {
    globalThis.fetch = originalFetch
  }
})

test('a rejected native submission reports the error without additional ACL writes', async () => {
  const originalFetch = globalThis.fetch
  const calls: string[] = []
  globalThis.fetch = (async (url) => {
    calls.push(String(url))
    return new Response('local contact blocks offered capabilities', { status: 409 })
  }) as typeof fetch
  try {
    let failure: unknown
    try {
      await api.sendContactRequest({ target_name: 'sky.lee~', description: '', requested_capabilities: [], offered_capabilities: ['dev.dhttp.net/message#message'] })
    } catch (error) {
      failure = error
    }
    expect(failure).toBeInstanceOf(ApiError)
    expect((failure as ApiError).status).toBe(409)
    expect(calls).toEqual(['/std/contact'])
  } finally {
    globalThis.fetch = originalFetch
  }
})
