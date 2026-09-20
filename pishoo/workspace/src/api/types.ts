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
  development_identity: boolean
  demo_data: boolean
  version: string
}

export interface Page<T> {
  items: T
  total: number
  page: number
  page_size: number
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

export interface Review {
  id: number
  visitor: string
  method: string
  api: string
  reason: string
  expired_after: string
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
