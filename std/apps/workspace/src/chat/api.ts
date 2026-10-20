import type { ChatMessage, ChatMessagePage } from '../api/types'
import { api, ApiError, backendPath, query, request } from '../api/client'

export const chatApi = {
  messages: (name: string, after?: string | null, limit = 50, signal?: AbortSignal) =>
    request<ChatMessagePage>(
      backendPath(`/message/list?${query({
        name,
        ...(after ? { after } : {}),
        limit,
      })}`),
      { signal },
    ),

  sendMessage: (name: string, content: string, clientMessageId: string) =>
    request<ChatMessage>(backendPath('/message'), {
      method: 'PUT',
      body: JSON.stringify({ to: name, content, media_type: 'text/plain', client_message_id: clientMessageId }),
    }),

  retryMessage: (index: number) =>
    request<ChatMessage>(backendPath(`/message/retry?${query({ index })}`), { method: 'PUT' }),

  grantCapability: async (name: string, capability: string) => {
    const catalog = await api.capabilities()
    const descriptor = catalog.find((entry) => entry.id === capability)
    if (!descriptor) {
      throw new ApiError(409, 'Capability or contact request changed')
    }
    const results = []
    for (const endpoint of descriptor.endpoints) {
      try { await api.setRule({ api: endpoint.path, method: endpoint.method, effect: 'allow', grantee: name }); results.push(`${endpoint.method} ${endpoint.path}: allowed`) }
      catch (error) { throw new Error(`${results.join('; ')}; ${endpoint.method} ${endpoint.path}: ${String(error)}`) }
    }
  },

  revokeCapability: async (name: string, capability: string) => {
    const descriptor = (await api.capabilities()).find((entry) => entry.id === capability)
    if (!descriptor) throw new ApiError(409, 'Capability changed')
    const results = []
    for (const endpoint of descriptor.endpoints) {
      try { await api.setRule({ api: endpoint.path, method: endpoint.method, effect: 'deny', grantee: name }); results.push(`${endpoint.method} ${endpoint.path}: denied`) }
      catch (error) { throw new Error(`${results.join('; ')}; ${endpoint.method} ${endpoint.path}: ${String(error)}`) }
    }
  },
}
