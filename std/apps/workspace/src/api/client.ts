import { contactStatus, normalizeIdentityName } from '../lib/format'

import type {
  ApiSummary,
  Approval,
  AccessApproval,
  CapabilityApproval,
  ChatCapabilityState,
  CapabilityDescriptor,
  Contact,
  DirectoryEntry,
  Introduction,
  OutboundContactInput,
  OutboundContactRequest,
  Page,
  ProfileSettings,
  PublicProfile,
  RuntimeContext,
  RuleRow,
  RulesByApi,
  RulesByGrantee,
} from './types'

export class ApiError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
  }
}

export async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers)
  if (init?.body && !headers.has('content-type')) {
    headers.set('content-type', 'application/json')
  }

  const response = await fetch(path, { ...init, headers })
  if (!response.ok) {
    const message = (await response.text()).trim()
    throw new ApiError(response.status, message || `Request failed (${response.status})`)
  }
  if (response.status === 204) {
    return undefined as T
  }
  return (await response.json()) as T
}

export function query(values: Record<string, string | number>): string {
  return new URLSearchParams(
    Object.entries(values).map(([key, value]) => [key, String(value)]),
  ).toString()
}

export function backendPath(path: string): string {
  if (path === '/message') return '/std/message'
  if (path.startsWith('/chat/')) return `/std/message/${path.slice(6)}`
  return `/std${path}`
}
function capabilityRequests(contacts: Contact[], catalog: CapabilityDescriptor[], rules: RulesByGrantee, expired: boolean): CapabilityApproval[] {
  const time = Date.now() / 1000
  return contacts.flatMap(contact => {
    const status = contactStatus(contact.status)
    if ((contact.expired_after <= time) !== expired || !['pending', 'transfered', 'active', 'expired'].includes(status)) return []
    const requestId = Number(Reflect.get(contact, 'id'))
    if (!Number.isSafeInteger(requestId) || requestId <= 0) throw new ApiError(502, 'Invalid contact request ID')
    return catalog.filter(capability => {
      const requested = capability.endpoints.some(endpoint => contact.requested_access[endpoint.path]?.includes(endpoint.method))
      const decided = capability.endpoints.every(endpoint => {
        // Capability decisions are written to native ACL rules, not contact.grants.
        const methods = rules[contact.name]?.[endpoint.path]
        return methods?.allow.includes(endpoint.method) || methods?.deny.includes(endpoint.method)
      })
      const awaitingAcceptance = ['pending', 'transfered'].includes(status)
        && capability.endpoints.some(endpoint => rules[contact.name]?.[endpoint.path]?.allow.includes(endpoint.method))
      return requested && (!decided || awaitingAcceptance)
    }).map(capability => ({ kind: 'capability' as const, request_id: requestId,
      contact_name: contact.name, capability_id: capability.id, capability_version: capability.version,
      requested_at: contact.created_at, expired_after: contact.expired_after }))
  })
}

export const api = {
  context: async (): Promise<RuntimeContext> => {
    const [profile, reviews, contacts, catalog, rules] = await Promise.all([
      request<ProfileSettings>('/std/profile'),
      request<Page<AccessApproval[]>>('/std/acl/reviews?page=1&page_size=1'),
      api.allContacts('name', 'asc'), api.capabilities(), api.rulesByGrantee(),
    ])
    return { profile: profile.identity_name, owner_name: profile.identity_name,
      badges: { pending_reviews: reviews.total, incoming_contacts: capabilityRequests(contacts, catalog, rules, false).length } }
  },

  introduction: (signal?: AbortSignal) => request<Introduction>('/std/profile/context', { signal }),

  capabilities: async (signal?: AbortSignal, target?: string): Promise<CapabilityDescriptor[]> => {
    const path = target ? `/std/dhttp/${encodeURIComponent(target)}/std/acl/promissions` : '/std/acl/promissions'
    const catalog = await request<unknown>(path, { signal })
    if (!catalog || typeof catalog !== 'object' || Array.isArray(catalog)) {
      throw new ApiError(502, 'Invalid permission catalog')
    }
    return Object.entries(catalog).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([title, paths]) => {
      if (!title.trim() || !paths || typeof paths !== 'object' || Array.isArray(paths)) {
        throw new ApiError(502, 'Invalid permission group')
      }
      const endpoints = Object.entries(paths).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).flatMap(([path, methods]) => {
        if (!path.startsWith('/std/') || !Array.isArray(methods)
          || methods.some(method => typeof method !== 'string' || !/^[!#$%&'*+.^_`|~0-9A-Z-]+$/.test(method))) {
          throw new ApiError(502, 'Invalid permission operations')
        }
        return [...new Set<string>(methods)].sort().map(method => ({ method, path }))
      })
      return { id: title, title, endpoints,
        // Compare actual operations before applying an approval; the wire format has no Lib version.
        version: JSON.stringify(endpoints), visibility: 'contact', approval_mode: 'capability', selectable: endpoints.length > 0 }
    })
  },

  approvals: async (status: 'pending' | 'expired', page: number, signal?: AbortSignal): Promise<Page<Approval[]>> => {
    const [contacts, catalog, rules] = await Promise.all([
      api.allContacts('name', 'asc', signal), api.capabilities(signal), api.rulesByGrantee(signal),
    ])
    const items: Approval[] = capabilityRequests(contacts, catalog, rules, status === 'expired')
    let sourcePage = 1
    while (true) {
      const result = await request<Page<AccessApproval[]>>(`/std/acl/reviews?${query({ status, page: sourcePage, page_size: 100 })}`, { signal })
      items.push(...result.items.map(review => {
        const expires: unknown = Reflect.get(review, 'expired_after')
        const requested: unknown = Reflect.get(review, 'requested_at')
        const seconds = (value: unknown) => typeof value === 'string' ? Date.parse(value) / 1000 : Number(value)
        const expiredAfter = seconds(expires), requestedAt = seconds(requested)
        if (!Number.isFinite(expiredAfter) || !Number.isFinite(requestedAt)) throw new ApiError(502, 'Invalid approval timestamp')
        return { ...review, kind: 'access' as const, expired_after: expiredAfter, requested_at: requestedAt }
      }))
      if (sourcePage * 100 >= result.total || !result.items.length) break
      sourcePage++
    }
    items.sort((a, b) => status === 'expired' ? b.expired_after - a.expired_after : b.requested_at - a.requested_at)
    return { items: items.slice((page - 1) * 20, page * 20), total: items.length, page, page_size: 20 }
  },

  deleteExpiredApproval: (approval: Approval) => approval.kind === 'capability'
    ? request<void>(`/std/contact/${encodeURIComponent(approval.contact_name)}`, { method: 'DELETE' })
    : request<void>(`/std/acl/review/${approval.id}`, { method: 'DELETE' }),

  outboundRequests: (page: number, signal?: AbortSignal) =>
    request<Page<OutboundContactRequest[]>>(`/std/contact/local/requests?${query({ page, page_size: 20 })}`, { signal }),

  outboundRequest: (id: number, signal?: AbortSignal) =>
    request<OutboundContactRequest>(`/std/contact/local/requests?${query({ id })}`, { signal }),

  sendContactRequest: async (value: OutboundContactInput) => {
    const target = normalizeIdentityName(value.target_name)
    if (!target) throw new ApiError(400, 'Invalid target name')
    if ([...value.description].length > 80 || /\p{Cc}/u.test(value.description)) {
      throw new ApiError(400, 'Invalid description')
    }
    return request<OutboundContactRequest>('/std/contact/local/requests', {
      method: 'POST', body: JSON.stringify({ ...value, target_name: target }),
    })
  },

  refreshOutboundRequest: (id: number) =>
    request<OutboundContactRequest>(`/std/contact/local/requests/refresh?${query({ id })}`, { method: 'POST' }),

  deleteOutboundRequest: (id: number) =>
    request<void>(`/std/contact/local/requests?${query({ id })}`, { method: 'DELETE' }),

  profileSettings: (signal?: AbortSignal) =>
    request<ProfileSettings>('/std/profile', { signal }),

  saveProfile: (displayName: string) =>
    request<ProfileSettings>('/std/profile', {
      method: 'PATCH',
      body: JSON.stringify({ display_name: displayName }),
    }),

  saveAvatar: (file: File) =>
    request<ProfileSettings>('/std/profile/avatar', {
      method: 'PUT',
      headers: { 'content-type': file.type },
      body: file,
    }),

  deleteAvatar: () =>
    request<ProfileSettings>('/std/profile/avatar', { method: 'DELETE' }),

  publicProfile: async (name: string, signal?: AbortSignal): Promise<PublicProfile> => {
    const target = normalizeIdentityName(name)
    if (!target) throw new ApiError(400, 'Invalid identity name')
    const deadline = AbortSignal.timeout(2500)
    const requestSignal = signal ? AbortSignal.any([signal, deadline]) : deadline
    const profile = await request<PublicProfile>(`/std/dhttp/${target}/std/profile`, { signal: requestSignal })
    return { ...profile, avatar_url: profile.avatar_url ? `/std/dhttp/${target}/std/profile/avatar` : null }
  },

  decideReview: (
    reviewId: number,
    action: 'allow' | 'deny',
    expiredAfter: string,
  ) =>
    request<void>('/std/acl/review', {
      method: 'PATCH',
      body: JSON.stringify({
        id: reviewId,
        action,
        expired_after: expiredAfter,
      }),
    }),

  contacts: (page: number, sort: string, order: string, signal?: AbortSignal, pageSize = 20) =>
    request<Page<Contact[]>>(
      `/std/contacts?${query({ page, page_size: pageSize, sort, order })}`,
      { signal },
    ),

  allContacts: async (sort: string, order: string, signal?: AbortSignal): Promise<Contact[]> => {
    const items: Contact[] = []
    let page = 1
    while (true) {
      const result = await api.contacts(page, sort, order, signal, 100)
      items.push(...result.items)
      if (items.length >= result.total || result.items.length === 0) return items
      page += 1
    }
  },

  contactDirectory: async (signal?: AbortSignal): Promise<DirectoryEntry[]> => {
    const [contacts, saved] = await Promise.all([api.allContacts('name', 'asc', signal), request<string[]>('/std/contact/local/saved', { signal })])
    const entries = await Promise.all(contacts.map(async contact => {
      let capability: Pick<ChatCapabilityState, 'can_send' | 'can_receive' | 'remote_grant'>
      try {
        capability = await request<ChatCapabilityState>(`/std/message/capability?${query({ name: contact.name })}`, { signal })
      } catch (error) {
        if (signal?.aborted) throw error
        capability = { can_send: false, can_receive: contact.granted_access['/std/message']?.allow.includes('POST') ?? false, remote_grant: null }
      }
      return { name: contact.name, saved: saved.includes(contact.name), chat_available: capability.can_send || capability.can_receive, remote_chat_granted: capability.remote_grant }
    }))
    return entries.filter(entry => entry.saved || entry.chat_available)
  },

  saveContactIdentity: (name: string) =>
    request<void>(`/std/contact/local/saved?${query({ name })}`, { method: 'PUT' }),

  unsaveContactIdentity: (name: string) =>
    request<void>(`/std/contact/local/saved?${query({ name })}`, { method: 'DELETE' }),

  contact: (name: string, signal?: AbortSignal) =>
    request<Contact>(`/std/contact/${encodeURIComponent(name)}`, { signal }),

  patchContact: (name: string, body: { status?: string; alias?: string }) =>
    request<void>(`/std/contact/${encodeURIComponent(name)}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    }),

  deleteContact: (name: string) =>
    request<void>(`/std/contact/${encodeURIComponent(name)}`, { method: 'DELETE' }),

  deleteContacts: (names: string[]) =>
    request<void>('/std/contacts', {
      method: 'DELETE',
      body: JSON.stringify({ names }),
    }),

  apiSummaries: (page: number, signal?: AbortSignal) =>
    request<Page<ApiSummary[]>>(
      `/std/acl/apis?${query({ page, page_size: 20, sort: 'api', order: 'asc' })}`,
      { signal },
    ),

  rulesByApi: (signal?: AbortSignal) => request<RulesByApi>('/std/acl/access', { signal }),

  rulesByGrantee: (signal?: AbortSignal) =>
    request<RulesByGrantee>('/std/acl/allow', { signal }),

  setRule: (rule: RuleRow) =>
    request<void>(`/std/acl/allow/${encodeURIComponent(rule.grantee)}`, {
      method: 'PATCH',
      body: JSON.stringify({ api: rule.api, method: rule.method, effect: rule.effect }),
    }),

  deleteRule: (rule: RuleRow) =>
    request<void>(`/std/acl/allow/${encodeURIComponent(rule.grantee)}`, {
      method: 'DELETE',
      body: JSON.stringify({ api: rule.api, method: rule.method }),
    }),
}
