import type { ChatCapabilityState, ChatMessage, ChatMessagePage } from '../api/types'
import { backendPath, query, request } from '../api/client'



export const chatApi = {
  messages: (name: string, after?: string | null, limit = 50, signal?: AbortSignal) =>
    request<ChatMessagePage>(
      backendPath(`/chat/messages?${query({
        name,
        ...(after ? { after } : {}),
        limit,
      })}`),
      { signal },
    ),

  sendMessage: (name: string, text: string) =>
    request<ChatMessage>(backendPath(`/chat/messages?${query({ name })}`), {
      method: 'POST',
      body: JSON.stringify({ text }),
    }),

  capability: (name: string, signal?: AbortSignal) =>
    request<ChatCapabilityState>(backendPath(`/chat/capability?${query({ name })}`), { signal }),

  grantCapability: (name: string, capability: string, reference?: { requestId: number; version: string }) =>
    request<void>(backendPath(`/contacts/capabilities/grant?${query({ name, capability, ...(reference ? { request_id: reference.requestId, capability_version: reference.version } : {}) })}`), {
      method: 'POST',
    }),

  revokeCapability: (name: string, capability: string) =>
    request<void>(backendPath(`/contacts/capabilities/revoke?${query({ name, capability })}`), {
      method: 'POST',
    }),

  denyCapability: (name: string, capability: string, requestId: number, capabilityVersion: string) =>
    request<void>(backendPath(`/contacts/capabilities/deny?${query({ name, capability, request_id: requestId, capability_version: capabilityVersion })}`), {
      method: 'POST',
    }),
}
