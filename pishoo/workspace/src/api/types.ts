export type Effect = 'allow' | 'review' | 'deny'
export type ContactStatus =
  | 'pending'
  | 'active'
  | 'transfered'
  | 'expired'
  | 'blocked'

export interface RuntimeContext {
  profile: string
  owner_name: string
  badges: {
    pending_reviews: number
    incoming_contacts: number | null
  }
}

export interface LibSummary {
  id: string
  title: string
  version: string
  description: string | null
  endpoints: Array<{ method: string; path: string; description: string | null }>
}

export interface Page<T> {
  items: T
  total: number
  page: number
  page_size: number
}

export interface ProfileSettings {
  identity_name: string
  display_name: string | null
  avatar_url: string | null
  updated_at: number
}

export interface PublicProfile {
  display_name: string | null
  avatar_url: string | null
  updated_at: number
}

export interface CapabilityDescriptor {
  id: string
  version: string
  visibility: 'public' | 'contact'
  approval_mode: 'none' | 'capability'
  selectable: boolean
  endpoints: Array<{ method: string; path: string }>
}

export type ChatMessageState = 'queued' | 'sending' | 'sent' | 'received' | 'failed' | 'blocked'

export interface ChatMessage {
  id: string
  client_message_id: string
  remote_message_id: string | null
  direction: 'incoming' | 'outgoing'
  state: ChatMessageState
  sender: string
  recipient: string
  text: string
  created_at: number
  updated_at: number
  error_message: string | null
}

export interface ChatMessagePage {
  items: ChatMessage[]
  next_cursor: string | null
}

export interface ChatCapabilityState {
  capability: 'chat'
  status: 'available' | 'waiting' | 'blocked'
  contact_status: ContactStatus
  can_send: boolean
  can_receive: boolean
  remote_grant: boolean | null
  endpoints: Array<{ method: string; path: string }>
}

export interface DirectoryEntry {
  name: string
  saved: boolean
  chat_available: boolean
  remote_chat_granted: boolean | null
}

export interface CapabilityApproval {
  kind: 'capability'
  request_id: number
  contact_name: string
  capability_id: 'chat'
  capability_version: string
  requested_at: number
  expired_after: number
}

export interface AccessApproval {
  kind: 'access'
  id: number
  visitor: string
  method: string
  api: string
  reason: string
  requested_at: number
  expired_after: number
}

export type Approval = CapabilityApproval | AccessApproval

export interface OutboundContactRequest {
  id: number
  target_name: string
  description: string
  requested_capabilities: string[]
  offered_capabilities: string[]
  status: 'queued' | 'pending' | 'active' | 'denied' | 'expired' | 'failed' | 'revoked'
  expired_after: number
  delivery_deadline: number
  remote_expired_after: number | null
  last_checked_at: number | null
  error_message: string | null
  created_at: number
  updated_at: number
}

export interface OutboundContactInput {
  target_name: string
  description: string
  requested_capabilities: string[]
  offered_capabilities: string[]
}

export interface GrantedMethods {
  allow: string[]
  review: string[]
  deny: string[]
}

export type RequestedAccess = Record<string, string[]>
export type GrantedAccess = Record<string, GrantedMethods>

export interface Contact {
  name: string
  /** Readable UTF-8 or `hex:` followed by the original bytes for binary subject IDs. */
  subject_id: string
  alias: string | null
  class: string
  description: string
  status: ContactStatus | number
  created_at: number
  updated_at: number
  expired_after: number
  requested_access: RequestedAccess
  granted_access: GrantedAccess
  offers: GrantedAccess
}

export interface ApiSummary {
  api: string
  updated_at: number
}

export interface EffectGrantees {
  allow: string[]
  review: string[]
  deny: string[]
}

export type RulesByApi = Record<string, Record<string, EffectGrantees>>
export type RulesByGrantee = Record<string, Record<string, GrantedMethods>>

export interface RuleRow {
  api: string
  method: string
  effect: Effect
  grantee: string
}
