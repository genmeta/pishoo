import { contactStatus, normalizeIdentityName } from '../lib/format'

import type {
  ApiSummary,
  Approval,
  AccessApproval,
  ContactApproval,
  CapabilityDescriptor,
  Contact,
  DirectoryEntry,
  Introduction,
  LibSummary,
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

interface ApiReview extends Omit<AccessApproval, 'kind' | 'review_kind' | 'expired_after'> {
  kind: 'live' | 'persistent'
  expired_after: string | number | null
}

export class ApiError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
  }
}

export class ReviewRequiredError extends ApiError {
  readonly reviewId?: number
  readonly statusUrl?: string

  constructor(reviewId?: number, statusUrl?: string) {
    super(400, 'need_review')
    this.name = 'ReviewRequiredError'
    this.reviewId = reviewId
    this.statusUrl = statusUrl
  }
}

export async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers)
  if (init?.body && !headers.has('content-type')) {
    headers.set('content-type', 'application/json')
  }

  if (!headers.has('if-reviewed')) headers.set('if-reviewed', 'break')
  const response = await fetch(path, { ...init, headers })
  if (!response.ok) {
    const message = (await response.text()).trim()
    let body: { error?: string; review_id?: number; status_url?: string } | undefined
    try { body = JSON.parse(message) } catch { /* Non-JSON errors keep their response text. */ }
    if (response.status === 400 && body?.error === 'need_review') {
      throw new ReviewRequiredError(body.review_id, body.status_url)
    }
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
  if (path.startsWith('/message/')) return `/std/message/${path.slice(9)}`
  return `/std${path}`
}
function contactRequests(contacts: Contact[], expired: boolean): ContactApproval[] {
  const time = Date.now() / 1000
  return contacts.filter(contact => contact.incoming === true
    && ['pending', 'transfered', 'expired'].includes(contactStatus(contact.status))
    && (contact.valid_before <= time) === expired).map(contact => ({
      kind: 'contact', request_id: Number(Reflect.get(contact, 'id')),
      contact_name: contact.name, subject_id: contact.subject_id,
      requested_at: contact.created_at, expired_after: contact.valid_before,
    }))
}

export const api = {
  context: async (): Promise<RuntimeContext> => {
    const [profile, reviews, contacts] = await Promise.all([
      request<ProfileSettings>('/std/profile'),
      request<Page<AccessApproval[]>>('/std/acl/reviews?page=1&page_size=1'),
      api.allContacts('name', 'asc'),
    ])
    return { profile: profile.identity_name, owner_name: profile.identity_name,
      badges: { pending_reviews: reviews.total, incoming_contacts: contactRequests(contacts, false).length } }
  },

  introduction: (signal?: AbortSignal) => request<Introduction>('/std/profile/context', { signal }),

  libs: (signal?: AbortSignal) => request<LibSummary[]>('/std/sys/package/loaded-libs', { signal }),

  capabilities: async (signal?: AbortSignal) => {
    const catalog = await request<CapabilityDescriptor[]>('/std/acl/capabilities', { signal })
    if (!Array.isArray(catalog)) throw new ApiError(502, 'Capability catalog unknown or awaiting access approval')
    return catalog
  },

  approvals: async (status: 'pending' | 'expired', page: number, signal?: AbortSignal): Promise<Page<Approval[]>> => {
    const contacts = await api.allContacts('name', 'asc', signal)
    const items: Approval[] = contactRequests(contacts, status === 'expired')
    let sourcePage = 1
    while (true) {
      const result = await request<Page<ApiReview[]>>(`/std/acl/reviews?${query({ status, page: sourcePage, page_size: 100 })}`, { signal })
      items.push(...result.items.map(review => {
        const expires = review.expired_after
        const requested = review.requested_at
        const seconds = (value: unknown) => typeof value === 'string' ? Date.parse(value) / 1000 : Number(value)
        const expiredAfter = expires === null ? null : seconds(expires), requestedAt = seconds(requested)
        if ((expiredAfter !== null && !Number.isFinite(expiredAfter)) || !Number.isFinite(requestedAt)) throw new ApiError(502, 'Invalid approval timestamp')
        return { ...review, kind: 'access' as const, review_kind: review.kind, expired_after: expiredAfter, requested_at: requestedAt }
      }))
      if (sourcePage * 100 >= result.total || !result.items.length) break
      sourcePage++
    }
    items.sort((a, b) => status === 'expired' ? (b.expired_after ?? 0) - (a.expired_after ?? 0) : b.requested_at - a.requested_at)
    return { items: items.slice((page - 1) * 20, page * 20), total: items.length, page, page_size: 20 }
  },

  deleteExpiredApproval: (approval: Approval) => approval.kind === 'contact'
    ? request<void>(`/std/contact?${query({ name: approval.contact_name })}`, { method: 'DELETE' })
    : request<void>(`/std/acl/review/${approval.id}`, { method: 'DELETE' }),

  outboundRequests: (page: number, signal?: AbortSignal) =>
    request<Page<OutboundContactRequest[]>>(`/std/contact/requests?${query({ page, page_size: 20 })}`, { signal }),

  outboundRequest: (id: number, signal?: AbortSignal) =>
    request<OutboundContactRequest>(`/std/contact/requests?${query({ id })}`, { signal }),

  sendContactRequest: async (value: OutboundContactInput) => {
    const target = normalizeIdentityName(value.target_name)
    if (!target) throw new ApiError(400, 'Invalid target name')
    if ([...value.description].length > 80 || /\p{Cc}/u.test(value.description)) {
      throw new ApiError(400, 'Invalid description')
    }
    return request<OutboundContactRequest>('/std/contact', {
      method: 'POST', body: JSON.stringify({ ...value, target_name: target }),
    })
  },

  refreshOutboundRequest: (id: number) =>
    request<OutboundContactRequest>(`/std/contact/requests/refresh?${query({ id })}`, { method: 'POST' }),

  deleteOutboundRequest: (id: number) =>
    request<void>(`/std/contact/requests?${query({ id })}`, { method: 'DELETE' }),

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
    kind: 'live' | 'persistent' = 'persistent',
  ) =>
    request<void>('/std/acl/review', {
      method: 'PATCH',
      body: JSON.stringify({
        id: reviewId,
        kind,
        action,
      }),
    }),

  contacts: (page: number, sort: string, order: string, signal?: AbortSignal, pageSize = 20) =>
    request<Page<Contact[]>>(
      `/std/contact/list?${query({ page, page_size: pageSize, sort, order })}`,
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

  contactProfile: async (name: string, signal?: AbortSignal): Promise<PublicProfile> => {
    const profile = await api.publicProfile(name, signal)
    return { ...profile, avatar_url: profile.avatar_url ? `/std/contact/${encodeURIComponent(name)}/profile/avatar` : null }
  },

  contactDirectory: async (signal?: AbortSignal): Promise<DirectoryEntry[]> => {
    const contacts = (await api.allContacts('name', 'asc', signal))
      .filter(contact => contactStatus(contact.status) !== 'blocked')
    return contacts.map(contact => ({
      ...contact,
      display_name: null,
      avatar_url: null,
      profile_updated_at: 0,
      chat_available: true,
    }))
  },

  enrichContactDirectory: async (
    contacts: DirectoryEntry[],
    onProfile: (name: string, profile: PublicProfile) => void,
    signal?: AbortSignal,
  ): Promise<void> => {
    let index = 0
    await Promise.all(Array.from({ length: Math.min(6, contacts.length) }, async () => {
      while (index < contacts.length) {
        const contact = contacts[index++]
        const profile = await api.contactProfile(contact.name, signal).catch(() => null)
        signal?.throwIfAborted()
        if (profile) onProfile(contact.name, profile)
      }
    }))
  },

  contact: (name: string, signal?: AbortSignal) =>
    request<Contact>(`/std/contact?${query({ name })}`, { signal }),

  patchContact: (name: string, body: { status?: string; alias?: string; expires_after?: number; valid_before?: number }) =>
    request<void>(`/std/contact?${query({ name })}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    }),

  deleteContact: (name: string) =>
    request<void>(`/std/contact?${query({ name })}`, { method: 'DELETE' }),

  deleteContacts: (names: string[]) =>
    request<void>('/std/contact', {
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
