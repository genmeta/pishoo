export type WorkspacePage =
  | 'home' | 'contacts' | 'contact-new' | 'contact-requests'
  | 'chat'
  | 'extensions' | 'apps' | 'approvals' | 'profile'
  | 'access' | 'capabilities' | 'not-found'

export interface WorkspaceRoute {
  page: WorkspacePage
  contactName?: string
}

export function routeFromPath(pathname: string): WorkspaceRoute {
  const path = pathname.replace(/\/$/, '') || '/'
  switch (path) {
    case '/std/workspace': return { page: 'home' }
    case '/std/workspace/contacts': return { page: 'contacts' }
    case '/std/workspace/contacts/new': return { page: 'contact-new' }
    case '/std/workspace/contacts/requests': return { page: 'contact-requests' }
    case '/std/workspace/extensions': return { page: 'extensions' }
    case '/std/workspace/apps': return { page: 'apps' }
    case '/std/workspace/approvals': return { page: 'approvals' }
    case '/std/workspace/settings/profile': return { page: 'profile' }
    case '/std/workspace/settings/access': return { page: 'access' }
    case '/std/workspace/settings/capabilities': return { page: 'capabilities' }
  }

  const chatMatch = /^\/std\/workspace\/contacts\/([^/]+)\/chat$/.exec(path)
  const match = chatMatch ?? /^\/std\/workspace\/contacts\/(?:requests\/)?([^/]+)$/.exec(path)
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
  return `/std/workspace/contacts/${encodeURIComponent(name)}`
}

export function contactChatPath(name: string): string {
  return `${contactPath(name)}/chat`
}
