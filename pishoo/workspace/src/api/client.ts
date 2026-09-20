import type {
  ApiSummary,
  Contact,
  Effect,
  Page,
  Review,
  RuntimeContext,
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

async function request<T>(path: string, init?: RequestInit): Promise<T> {
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

function query(values: Record<string, string | number>): string {
  return new URLSearchParams(
    Object.entries(values).map(([key, value]) => [key, String(value)]),
  ).toString()
}

function apiTarget(api: string): string {
  return api
    .replace(/^\/+/, '')
    .split('/')
    .filter(Boolean)
    .map(encodeURIComponent)
    .join('/')
}

export const api = {
  context: () => request<RuntimeContext>('/workspace-api/context'),

  reviews: (page: number, signal?: AbortSignal) =>
    request<Page<Review[]>>(`/acl/reviews?${query({ page, page_size: 20 })}`, {
      signal,
    }),

  decideReview: (
    reviewId: number,
    action: 'allow' | 'deny',
    expiredAfter: string,
  ) =>
    request<void>('/acl/review', {
      method: 'PATCH',
      body: JSON.stringify({
        id: reviewId,
        action,
        expired_after: expiredAfter,
      }),
    }),

  contacts: (page: number, sort: string, order: string, signal?: AbortSignal) =>
    request<Page<Contact[]>>(
      `/contacts?${query({ page, page_size: 20, sort, order })}`,
      { signal },
    ),

  contact: (name: string, signal?: AbortSignal) =>
    request<Contact>(`/contact/${encodeURIComponent(name)}`, { signal }),

  patchContact: (name: string, body: { status?: string; alias?: string }) =>
    request<void>(`/contact/${encodeURIComponent(name)}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    }),

  deleteContact: (name: string) =>
    request<void>(`/contact/${encodeURIComponent(name)}`, { method: 'DELETE' }),

  deleteContacts: (names: string[]) =>
    request<void>('/contacts', {
      method: 'DELETE',
      body: JSON.stringify({ names }),
    }),

  apiSummaries: (page: number, signal?: AbortSignal) =>
    request<Page<ApiSummary[]>>(
      `/acl/apis?${query({ page, page_size: 20, sort: 'api', order: 'asc' })}`,
      { signal },
    ),

  rulesByApi: (signal?: AbortSignal) => request<RulesByApi>('/acl/access', { signal }),

  rulesByGrantee: (signal?: AbortSignal) =>
    request<RulesByGrantee>('/acl/allow', { signal }),

  setRule: (rule: { api: string; method: string; effect: Effect; grantee: string }) =>
    request<void>(`/acl/access/${apiTarget(rule.api)}`, {
      method: 'POST',
      body: JSON.stringify({
        method: rule.method,
        effect: rule.effect,
        grantee: rule.grantee,
      }),
    }),

  deleteRule: (rule: { api: string; method: string; grantee: string }) =>
    request<void>(`/acl/allow/${encodeURIComponent(rule.grantee)}`, {
      method: 'DELETE',
      body: JSON.stringify({ method: rule.method, api: rule.api }),
    }),
}
