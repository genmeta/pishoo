import type { Contact } from '../api/types'
import type { MessageKey, Translator } from '../i18n'
import { contactStatus } from './format'

export type ChatDirectionState = 'enabled' | 'waiting' | 'not-granted' | 'unknown' | 'unavailable'
const CHAT_DIRECTION_KEYS: Record<ChatDirectionState, MessageKey> = {
  enabled: 'contacts.chatDirection.enabled', waiting: 'contacts.chatDirection.waiting',
  'not-granted': 'contacts.chatDirection.notGranted', unknown: 'contacts.chatDirection.unknown',
  unavailable: 'contacts.chatDirection.unavailable',
}
export function chatCapabilityLabel(contact: Contact, t: Translator): string {
  return t(contactStatus(contact.status) === 'blocked' ? 'contacts.chatInactive' : 'contacts.chatAttempt')
}
export function chatDirectionState(direction: 'incoming' | 'outgoing', contact: Contact): ChatDirectionState {
  if (contactStatus(contact.status) === 'blocked') return 'unavailable'
  if (direction === 'outgoing') return 'unknown'
  const methods = contact.granted_access['/std/message']
  if (methods?.allow.some(method => method === 'POST' || method === '*')) return 'enabled'
  if (methods?.review.some(method => method === 'POST' || method === '*')) return 'waiting'
  if (methods?.deny.some(method => method === 'POST' || method === '*')) return 'not-granted'
  return 'unknown'
}
export function chatDirectionLabel(state: ChatDirectionState, t: Translator): string { return t(CHAT_DIRECTION_KEYS[state]) }
