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
    case '/workspace': return { page: 'home' }
    case '/workspace/contacts': return { page: 'contacts' }
    case '/workspace/contacts/new': return { page: 'contact-new' }
    case '/workspace/contacts/requests': return { page: 'contact-requests' }
    case '/workspace/extensions': return { page: 'extensions' }
    case '/workspace/apps': return { page: 'apps' }
    case '/workspace/approvals': return { page: 'approvals' }
    case '/workspace/settings/profile': return { page: 'profile' }
    case '/workspace/settings/access': return { page: 'access' }
    case '/workspace/settings/capabilities': return { page: 'capabilities' }
  }

  const chatMatch = /^\/workspace\/contacts\/([^/]+)\/chat$/.exec(path)
  const match = chatMatch ?? /^\/workspace\/contacts\/(?:requests\/)?([^/]+)$/.exec(path)
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
  return `/workspace/contacts/${encodeURIComponent(name)}`
}

export function contactChatPath(name: string): string {
  return `${contactPath(name)}/chat`
}
