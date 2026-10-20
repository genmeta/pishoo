import { afterEach, expect, test } from 'bun:test'

import { api } from '../src/api/client'
import type { Contact } from '../src/api/types'

const originalFetch = globalThis.fetch
afterEach(() => { globalThis.fetch = originalFetch })

function contact(name: string, status: Contact['status'] = 'active', alias: string | null = null): Contact {
  return { name, status, alias, class: 'Human', description: '', subject_id: 'a'.repeat(64),
    created_at: 1, updated_at: 1, valid_before: 2000000000, requested_access: {}, granted_access: {}, offers: {} }
}

test('directory returns local contacts before remote profile enrichment', async () => {
  const contacts = [contact('alice.dhttp.net', 'active', 'Zulu'), contact('bob.dhttp.net', 'pending'),
    contact('carol.dhttp.net'), contact('dave.dhttp.net', 'blocked')]
  const paths: string[] = []
  globalThis.fetch = (async (input) => {
    const url = new URL(String(input), 'https://localhost')
    paths.push(url.pathname)
    if (url.pathname === '/std/contact/list') return Response.json({ items: contacts, total: contacts.length })
    if (url.pathname.endsWith('/std/profile')) return Response.json({
      display_name: url.pathname.includes('bob.') ? 'Alpha' : 'Beta', avatar_url: '/std/profile/avatar', updated_at: 42,
    })
    throw new Error(`Unexpected request ${url}`)
  }) as typeof fetch
  const directory = await api.contactDirectory()
  expect(directory.map(entry => entry.name)).toEqual(['alice.dhttp.net', 'bob.dhttp.net', 'carol.dhttp.net'])
  expect(directory.every(entry => entry.chat_available)).toBe(true)
  expect(directory.every(entry => entry.display_name === null && entry.avatar_url === null)).toBe(true)
  expect(paths).toEqual(['/std/contact/list'])
  const enriched: string[] = []
  await api.enrichContactDirectory(directory, (name, profile) => {
    enriched.push(`${name}:${profile.display_name}`)
  })
  expect(enriched).toHaveLength(3)
  expect(enriched).toContain('bob.dhttp.net:Alpha')
  expect(directory[1].avatar_url).toBe(null)
  expect(paths.some(path => path.includes('/local/') || path.includes('dave.'))).toBe(false)
})

test('failed peer discovery preserves the locally loaded contacts', async () => {
  const contacts = [contact('a.dhttp.net'), contact('z.dhttp.net')]
  globalThis.fetch = (async (input) => new URL(String(input), 'https://localhost').pathname === '/std/contact/list'
    ? Response.json({ items: contacts, total: contacts.length })
    : new Response('offline', { status: 502 })) as typeof fetch
  expect((await api.contactDirectory()).map(entry => entry.name)).toEqual(['a.dhttp.net', 'z.dhttp.net'])
})

test('contact operations share one management resource and put the target in the query', async () => {
  const calls: Array<{ path: string; method: string }> = []
  globalThis.fetch = (async (input, init) => {
    calls.push({ path: String(input), method: init?.method ?? 'GET' })
    return init?.method ? new Response(null, { status: 204 }) : Response.json(contact('alice.dhttp.net'))
  }) as typeof fetch
  await api.contact('alice.dhttp.net')
  await api.patchContact('alice.dhttp.net', { alias: 'Alice' })
  await api.deleteContact('alice.dhttp.net')
  await api.deleteContacts(['alice.dhttp.net', 'bob.dhttp.net'])
  expect(calls).toEqual([
    { path: '/std/contact?name=alice.dhttp.net', method: 'GET' },
    { path: '/std/contact?name=alice.dhttp.net', method: 'PATCH' },
    { path: '/std/contact?name=alice.dhttp.net', method: 'DELETE' },
    { path: '/std/contact', method: 'DELETE' },
  ])
})
