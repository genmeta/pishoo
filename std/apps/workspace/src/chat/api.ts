import type { ChatCapabilityState, ChatMessage, ChatMessagePage } from '../api/types'
import { api, ApiError, backendPath, query, request } from '../api/client'
import { contactStatus } from '../lib/format'



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

  grantCapability: async (name: string, capability: string, reference?: { requestId: number; version: string }) => {
    const [catalog, contact] = await Promise.all([api.capabilities(), api.contact(name)])
    const descriptor = catalog.find((entry) => entry.id === capability)
    if (!descriptor || (reference && ((contact as typeof contact & { id: number }).id !== reference.requestId || descriptor.version !== reference.version))) {
      throw new ApiError(409, 'Capability or contact request changed')
    }
    const results = []
    for (const endpoint of descriptor.endpoints) {
      try { await api.setRule({ api: endpoint.path, method: endpoint.method, effect: 'allow', grantee: name }); results.push(`${endpoint.method} ${endpoint.path}: allowed`) }
      catch (error) { throw new Error(`${results.join('; ')}; ${endpoint.method} ${endpoint.path}: ${String(error)}`) }
    }
    // daccess reports completion to the applicant only after accepting the contact.
    if (['pending', 'transfered'].includes(contactStatus(contact.status))) {
      try {
        await api.patchContact(name, { status: 'active' })
      } catch (error) {
        if (!(error instanceof ApiError) || error.status !== 409) throw error
        // Crossed requests can be reconciled while ACL writes are in flight.
        const current = await api.contact(name)
        if (Reflect.get(current, 'id') !== Reflect.get(contact, 'id')
          || current.subject_id !== contact.subject_id
          || contactStatus(current.status) !== 'active') throw error
      }
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

  denyCapability: async (name: string, capability: string, requestId: number, capabilityVersion: string) => {
    const [catalog, contact] = await Promise.all([api.capabilities(), api.contact(name)])
    const descriptor = catalog.find((entry) => entry.id === capability)
    if (!descriptor || (contact as typeof contact & { id: number }).id !== requestId || descriptor.version !== capabilityVersion) throw new ApiError(409, 'Capability or contact request changed')
    const results = []
    for (const endpoint of descriptor.endpoints) {
      try { await api.setRule({ api: endpoint.path, method: endpoint.method, effect: 'deny', grantee: name }); results.push(`${endpoint.method} ${endpoint.path}: denied`) }
      catch (error) { throw new Error(`${results.join('; ')}; ${endpoint.method} ${endpoint.path}: ${String(error)}`) }
    }
  },
}
