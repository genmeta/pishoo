import type {
  ApiSummary,
  Approval,
  CapabilityDescriptor,
  Contact,
  DirectoryEntry,
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

export const api = {
  context: () => request<RuntimeContext>('/std/workspace-api/context'),

  libs: (signal?: AbortSignal) => request<LibSummary[]>('/std/workspace-api/libs', { signal }),

  capabilities: (signal?: AbortSignal) =>
    request<CapabilityDescriptor[]>('/std/workspace-api/capabilities', { signal }),

  approvals: (status: 'pending' | 'expired', page: number, signal?: AbortSignal) =>
    request<Page<Approval[]>>(`/std/workspace-api/approvals?${query({ status, page, page_size: 20 })}`, { signal }),

  deleteExpiredApproval: (approval: Approval) => {
    const id = approval.kind === 'capability' ? approval.request_id : approval.id
    return request<void>(`/std/workspace-api/approvals/${approval.kind}/${encodeURIComponent(id)}`, { method: 'DELETE' })
  },

  outboundRequests: (page: number, signal?: AbortSignal) =>
    request<Page<OutboundContactRequest[]>>(`/std/workspace-api/contact-requests?${query({ page, page_size: 20 })}`, { signal }),

  outboundRequest: (id: number, signal?: AbortSignal) =>
    request<OutboundContactRequest>(`/std/workspace-api/contact-requests/${encodeURIComponent(id)}`, { signal }),

  sendContactRequest: (value: OutboundContactInput) =>
    request<OutboundContactRequest>('/std/workspace-api/contact-requests', { method: 'POST', body: JSON.stringify(value) }),

  refreshOutboundRequest: (id: number) =>
    request<OutboundContactRequest>(`/std/workspace-api/contact-requests/${encodeURIComponent(id)}/refresh`, { method: 'POST' }),

  deleteOutboundRequest: (id: number) =>
    request<void>(`/std/workspace-api/contact-requests/${encodeURIComponent(id)}`, { method: 'DELETE' }),

  profileSettings: (signal?: AbortSignal) =>
    request<ProfileSettings>('/std/workspace-api/settings/profile', { signal }),

  saveProfile: (displayName: string) =>
    request<ProfileSettings>('/std/workspace-api/settings/profile', {
      method: 'PATCH',
      body: JSON.stringify({ display_name: displayName }),
    }),

  saveAvatar: (file: File) =>
    request<ProfileSettings>('/std/workspace-api/settings/profile/avatar', {
      method: 'PUT',
      headers: { 'content-type': file.type },
      body: file,
    }),

  deleteAvatar: () =>
    request<ProfileSettings>('/std/workspace-api/settings/profile/avatar', { method: 'DELETE' }),

  publicProfile: (name: string, signal?: AbortSignal) => {
    const deadline = AbortSignal.timeout(2500)
    const requestSignal = signal ? AbortSignal.any([signal, deadline]) : deadline
    return request<PublicProfile>(`/std/workspace-api/profiles/${encodeURIComponent(name)}`, { signal: requestSignal })
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

  contactDirectory: (signal?: AbortSignal) =>
    request<DirectoryEntry[]>('/std/workspace-api/contact-directory', { signal }),

  saveContactIdentity: (name: string) =>
    request<void>(`/std/workspace-api/contacts/${encodeURIComponent(name)}/saved`, { method: 'PUT' }),

  unsaveContactIdentity: (name: string) =>
    request<void>(`/std/workspace-api/contacts/${encodeURIComponent(name)}/saved`, { method: 'DELETE' }),

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
