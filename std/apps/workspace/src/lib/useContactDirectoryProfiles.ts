import { createEffect, createMemo, createSignal, onCleanup, type Accessor } from 'solid-js'

import { api } from '../api/client'
import type { DirectoryEntry, PublicProfile } from '../api/types'
import { compareContacts } from './format'

export function useContactDirectoryProfiles(
  source: Accessor<DirectoryEntry[] | undefined>,
): Accessor<DirectoryEntry[]> {
  const [profiles, setProfiles] = createSignal(new Map<string, PublicProfile>())
  const requested = new Set<string>()
  const controllers = new Set<AbortController>()
  const frames = new Set<number>()
  let disposed = false

  onCleanup(() => {
    disposed = true
    frames.forEach(frame => window.cancelAnimationFrame(frame))
    controllers.forEach(controller => controller.abort())
  })

  createEffect(() => {
    const entries = source()
    if (!entries) return

    const pending = entries.filter(entry => !requested.has(entry.name))
    if (!pending.length) return
    pending.forEach(entry => requested.add(entry.name))

    const frame = window.requestAnimationFrame(() => {
      frames.delete(frame)
      if (disposed) return

      const controller = new AbortController()
      controllers.add(controller)
      void api.enrichContactDirectory(pending, (name, profile) => {
        if (disposed) return
        setProfiles(current => new Map(current).set(name, profile))
      }, controller.signal).catch(() => {}).finally(() => controllers.delete(controller))
    })
    frames.add(frame)
  })

  return createMemo(() => {
    const currentProfiles = profiles()
    return (source() ?? []).map(entry => {
      const profile = currentProfiles.get(entry.name)
      return profile ? {
        ...entry,
        display_name: profile.display_name,
        avatar_url: profile.avatar_url,
        profile_updated_at: profile.updated_at,
      } : entry
    }).sort(compareContacts)
  })
}
