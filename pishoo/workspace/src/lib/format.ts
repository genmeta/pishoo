import type { ContactStatus } from '../api/types'

const DHTTP_NAME_SUFFIX = '.dhttp.net'

const CONTACT_STATUS: Record<number, ContactStatus> = {
  0: 'pending',
  1: 'pending',
  2: 'active',
  3: 'transfered',
  4: 'expired',
  5: 'blocked',
}

export function contactStatus(status: ContactStatus | number): ContactStatus {
  return typeof status === 'number' ? (CONTACT_STATUS[status] ?? 'pending') : status
}

export function initials(value: string): string {
  const parts = value.split(/[.@\s_-]+/).filter(Boolean)
  return parts
    .slice(0, 2)
    .map((part) => part[0]?.toUpperCase())
    .join('')
}

export function displayIdentityName(value: string): string {
  return value.endsWith(DHTTP_NAME_SUFFIX)
    ? value.slice(0, -DHTTP_NAME_SUFFIX.length)
    : value
}

export function errorMessage(error: unknown, fallback: string): string {
  return error instanceof Error ? error.message : fallback
}
