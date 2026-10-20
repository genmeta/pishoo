import { normalizeIdentityName } from './format'

export type WorkspacePage =
  | 'home' | 'contacts' | 'contact-new' | 'contact-requests'
  | 'chat'
  | 'apps' | 'app-detail' | 'approvals' | 'profile'
  | 'access' | 'capabilities' | 'not-found'

export interface WorkspaceRoute {
  page: WorkspacePage
  contactName?: string
}

export function routeFromPath(pathname: string): WorkspaceRoute {
  const path = pathname.replace(/^\/workspace(?=\/|$)/, '').replace(/\/$/, '') || '/'
  switch (path) {
    case '/': return { page: 'home' }
    case '/contacts': return { page: 'contacts' }
    case '/contacts/new': return { page: 'contact-new' }
    case '/contacts/requests': return { page: 'contact-requests' }
    case '/apps': return { page: 'apps' }
    case '/approvals': return { page: 'approvals' }
    case '/settings/approvals': return { page: 'approvals' }
    case '/settings/profile': return { page: 'profile' }
    case '/settings/access': return { page: 'access' }
    case '/settings/capabilities': return { page: 'capabilities' }
  }

  // Resource validation belongs to the detail page; malformed IDs never become API requests.
  if (path.startsWith('/apps/')) return { page: 'app-detail' }

  const chatMatch = /^\/contacts\/([^/]+)\/chat$/.exec(path)
  const match = chatMatch ?? /^\/contacts\/(?:requests\/)?([^/]+)$/.exec(path)
  if (match) {
    try {
      const contactName = decodeURIComponent(match[1])
      if (contactName && !contactName.includes('/')) {
        return { page: chatMatch ? 'chat' : 'contacts', contactName }
      }
    } catch {
      // Invalid URL escapes must not crash navigation.
    }
  }
  return { page: 'not-found' }
}

export function contactPath(name: string): string {
  return workspacePath(`/contacts/${encodeURIComponent(name)}`)
}

export function contactChatPath(name: string): string {
  return `${contactPath(name)}/chat`
}

export function workspacePath(path: string): string {
  return /^\/workspace(?:\/|\?|$)/.test(path) ? path : `/workspace${path || '/'}`
}

export function addContactUrl(viewerName: string, targetName: string): string | null {
  const viewer = normalizeIdentityName(viewerName)
  const target = normalizeIdentityName(targetName)
  if (!viewer || !target || viewer === target) return null
  const url = new URL(`https://${viewer}/workspace/contacts/new`)
  url.search = new URLSearchParams({ target }).toString()
  return url.href
}
