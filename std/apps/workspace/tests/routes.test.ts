import { expect, test } from 'bun:test'

import { addContactUrl, contactChatPath, contactPath, routeFromPath, workspacePath } from '../src/lib/routes'

test('Workspace pathname routes preserve contact identity and use no navigation hash', () => {
  expect(routeFromPath('/workspace/contacts')).toEqual({ page: 'contacts' })
  expect(routeFromPath('/workspace/contacts/alice.dhttp.net/chat')).toEqual({ page: 'chat', contactName: 'alice.dhttp.net' })
  expect(routeFromPath('/workspace/apps/local/workspace')).toEqual({ page: 'app-detail' })
  expect(routeFromPath('/workspace/')).toEqual({ page: 'home' })
  expect(routeFromPath('/workspace/contacts/%2Foutside')).toEqual({ page: 'not-found' })
  expect(contactPath('alice.dhttp.net')).toBe('/workspace/contacts/alice.dhttp.net')
  expect(contactChatPath('alice.dhttp.net')).toBe('/workspace/contacts/alice.dhttp.net/chat')
})

test('Workspace links preserve queries and keep an existing mount prefix', () => {
  expect(workspacePath('/apps?kind=lib&name=message')).toBe('/workspace/apps?kind=lib&name=message')
  expect(workspacePath('/workspace/apps?kind=app')).toBe('/workspace/apps?kind=app')
  expect(addContactUrl('bob', 'alice')).toBe('https://bob.dhttp.net/workspace/contacts/new?target=alice.dhttp.net')
  expect(addContactUrl('alice', 'alice.dhttp.net')).toBeNull()
})
