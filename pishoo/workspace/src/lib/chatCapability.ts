import type { ChatCapabilityState, Contact } from '../api/types'
import type { MessageKey, Translator } from '../i18n'
import { contactStatus } from './format'

export type ChatDirectionState = 'enabled' | 'waiting' | 'not-granted' | 'unknown' | 'unavailable'

const CHAT_DIRECTION_KEYS: Record<ChatDirectionState, MessageKey> = {
  enabled: 'contacts.chatDirection.enabled',
  waiting: 'contacts.chatDirection.waiting',
  'not-granted': 'contacts.chatDirection.notGranted',
  unknown: 'contacts.chatDirection.unknown',
  unavailable: 'contacts.chatDirection.unavailable',
}

export function chatCapabilityLabel(contact: Contact, t: Translator, capability?: ChatCapabilityState): string {
  if (['blocked', 'expired'].includes(contactStatus(contact.status))) {
    return t('contacts.chatInactive')
  }
  if (capability) {
    if (capability.can_receive) return t('contacts.chatEnabled')
    if (capability.remote_grant === true) return t('contacts.chatRemoteEnabled')
    if (capability.can_send) return t('contacts.chatRequested')
    return t('contacts.chatUnavailable')
  }
  if (Object.hasOwn(contact.granted_access, '/std/message')) return t('contacts.chatEnabled')
  if (Object.hasOwn(contact.requested_access, '/std/message')) return t('contacts.chatRequested')
  return t('contacts.chatNotRequested')
}

export function chatDirectionState(
  direction: 'incoming' | 'outgoing',
  contact: Contact,
  capability: ChatCapabilityState | undefined,
): ChatDirectionState {
  const status = contactStatus(contact.status)
  if (status === 'blocked' || status === 'expired') return 'unavailable'
  const requested = Object.hasOwn(contact.requested_access, '/std/message')
  if (direction === 'incoming') {
    if (capability) {
      if (capability.can_receive === true) return 'enabled'
      return ['pending', 'transfered'].includes(status) && requested ? 'waiting' : 'unavailable'
    }
    if (Object.hasOwn(contact.granted_access, '/std/message')) return 'enabled'
    return requested ? 'waiting' : 'unavailable'
  }
  if (capability) {
    if (['pending', 'transfered'].includes(status)) return 'waiting'
    if (capability.remote_grant === true) return 'enabled'
    if (capability.remote_grant === false) return 'not-granted'
    return capability.can_send ? 'unknown' : 'unavailable'
  }
  return requested ? 'waiting' : 'unavailable'
}

export function chatDirectionLabel(state: ChatDirectionState, t: Translator): string {
  return t(CHAT_DIRECTION_KEYS[state])
}
