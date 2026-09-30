import type { ChatCapabilityState, ChatMessage, ChatMessagePage } from '../api/types'
import { query, request } from '../api/client'

function conversationPath(name: string): string {
  return `/chat-api/conversations/${encodeURIComponent(name)}`
}

export const chatApi = {
  messages: (name: string, after?: string | null, limit = 50, signal?: AbortSignal) =>
    request<ChatMessagePage>(
      `${conversationPath(name)}/messages?${query({
        ...(after ? { after } : {}),
        limit,
      })}`,
      { signal },
    ),

  sendMessage: (name: string, text: string) =>
    request<ChatMessage>(`${conversationPath(name)}/messages`, {
      method: 'POST',
      body: JSON.stringify({ text }),
    }),

  capability: (name: string, signal?: AbortSignal) =>
    request<ChatCapabilityState>(`${conversationPath(name)}/capability`, { signal }),

  grantCapability: (name: string, capability: string, reference?: { requestId: number; version: string }) =>
    request<void>(`/workspace-api/contacts/${encodeURIComponent(name)}/capabilities/${encodeURIComponent(capability)}/grant${reference ? `?${query({ request_id: reference.requestId, capability_version: reference.version })}` : ''}`, {
      method: 'POST',
    }),

  revokeCapability: (name: string, capability: string) =>
    request<void>(`/workspace-api/contacts/${encodeURIComponent(name)}/capabilities/${encodeURIComponent(capability)}/revoke`, {
      method: 'POST',
    }),

  denyCapability: (name: string, capability: string, requestId: number, capabilityVersion: string) =>
    request<void>(`/workspace-api/contacts/${encodeURIComponent(name)}/capabilities/${encodeURIComponent(capability)}/deny?${query({ request_id: requestId, capability_version: capabilityVersion })}`, {
      method: 'POST',
    }),
}
