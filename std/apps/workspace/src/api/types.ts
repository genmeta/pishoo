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
  repository?: string
  kind?: 'app' | 'lib'
  dependencies?: Array<{ publisher: string; name: string; version: string }>
  id: string
  title: string
  version: string
  description: string | null
  endpoints: Array<{ method: string; path: string; description: string | null }>
  license?: string
  entry?: string
  changelog?: string
  host?: Partial<Record<'private-storage' | 'request-identity' | 'signatures', string>>
  openapi?: {
    openapi: string
    info: { title: string; version: string; description?: string; license?: { name: string } }
    'x-pishoo'?: {
      kind?: 'app' | 'lib'
      dependencies?: Array<{ publisher: string; name: string; version: string }>
      publisher?: string
      name?: string
      entry?: string
      changelog?: string
      host?: Partial<Record<'private-storage' | 'request-identity' | 'signatures', string>>
      'access-groups'?: Record<string, { title: string; operations: string[] }>
    }
    paths?: Record<string, Partial<Record<'get' | 'post' | 'put' | 'patch' | 'delete' | 'head' | 'options' | 'trace', {
      operationId?: string
      summary?: string
      description?: string
      'x-pishoo'?: {
        'access-group'?: string
        'initial-access'?: Array<{ grantee: string; effect: Effect }>
      }
    }>>>
  }
}

export interface Page<T> {
  items: T
  total: number
  page: number
  page_size: number
}

export interface StoreLib {
  kind?: 'app' | 'lib'
  dependencies?: Array<{ publisher: string; name: string; version: string }>
  id: string
  publisher: string
  publisher_owner_hash: string
  name: string
  version: string
  status: 'published' | 'yanked'
  status_reason: string
  published_at: number
  package_sha256: string
  size: number
  download_path: string
}

export interface StoreSummary extends StoreLib {
  repository?: string
  license?: string
  likes_count?: number
  favorites_count?: number
  downloads_count?: number
  title: string
  description: string | null
  changelog: string | null
  host: NonNullable<NonNullable<LibSummary['openapi']>['x-pishoo']>['host'] | null
}

export interface StoreDetail extends StoreSummary {
  openapi?: NonNullable<LibSummary['openapi']>
  signer_certificates: string[]
}

export interface StoreCatalog {
  libs?: StoreSummary[]
  apps?: StoreSummary[]
  page: number
  page_size: number
  total: number
}

export interface StoreVersions {
  versions: StoreSummary[]
  page: number
  page_size: number
}

export type StoreUpdate =
  | { id: string; status: 'current' | 'not_found' | 'unavailable' }
  | { id: string; status: 'update'; version: string; detail_path: string; download_path: string; changelog: string | null; host: StoreSummary['host'] }

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

export interface Introduction extends PublicProfile {
  identity_name: string
  viewer_name: string | null
  is_owner: boolean
}

export interface CapabilityDescriptor {
  id: string
  title: string
  /** Canonical operation snapshot used to detect changes while approving. Not a Lib version. */
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
  capability: 'message'
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
  capability_id: string
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
