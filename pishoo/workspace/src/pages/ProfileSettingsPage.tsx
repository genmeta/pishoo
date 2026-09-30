import { Show, createEffect, createSignal } from 'solid-js'

import { ApiError, api } from '../api/client'
import type { ProfileSettings } from '../api/types'
import { Avatar, ErrorState, LoadingState } from '../components/Ui'
import { useI18n } from '../i18n'
import { displayIdentityName, errorMessage, profileAvatarUrl } from '../lib/format'

export default function ProfileSettingsPage(props: {
  profile: ProfileSettings | undefined
  loading: boolean
  error: unknown
  ownerName: string
  refresh: () => void
  onSaved: (value: ProfileSettings) => void
  notify: (message: string, tone?: 'success' | 'error') => void
}) {
  const { date, t } = useI18n()
  const [displayName, setDisplayName] = createSignal('')
  const [fieldError, setFieldError] = createSignal<string | null>(null)
  const [avatarError, setAvatarError] = createSignal<string | null>(null)
  const [saving, setSaving] = createSignal(false)
  const [savingAvatar, setSavingAvatar] = createSignal(false)

  createEffect(() => setDisplayName(props.profile?.display_name ?? ''))

  const validate = () => {
    const name = displayName().trim()
    if ([...name].length > 80 || /\p{Cc}/u.test(name)) {
      setFieldError(t('profile.invalidName'))
      return false
    }
    setFieldError(null)
    return true
  }

  const save = async () => {
    if (saving() || !validate()) return
    setSaving(true)
    try {
      const updated = await api.saveProfile(displayName())
      props.onSaved(updated)
      props.notify(t('profile.saved'))
    } catch (error) {
      setFieldError(errorMessage(error, t('common.unexpectedError')))
    } finally {
      setSaving(false)
    }
  }

  const avatarUrl = () => profileAvatarUrl(props.profile)

  const uploadAvatar = async (file: File | undefined) => {
    if (!file || savingAvatar()) return
    if (!['image/jpeg', 'image/png', 'image/webp'].includes(file.type) || file.size > 1024 * 1024) {
      setAvatarError(t('profile.invalidAvatar'))
      return
    }
    setSavingAvatar(true)
    setAvatarError(null)
    try {
      props.onSaved(await api.saveAvatar(file))
      props.notify(t('profile.avatarSaved'))
    } catch (error) {
      setAvatarError(error instanceof ApiError && [400, 413, 415].includes(error.status)
        ? t('profile.invalidAvatar')
        : errorMessage(error, t('common.unexpectedError')))
    } finally {
      setSavingAvatar(false)
    }
  }

  const removeAvatar = async () => {
    if (savingAvatar()) return
    setSavingAvatar(true)
    setAvatarError(null)
    try {
      props.onSaved(await api.deleteAvatar())
      props.notify(t('profile.avatarRemoved'))
    } catch (error) {
      setAvatarError(errorMessage(error, t('common.unexpectedError')))
    } finally {
      setSavingAvatar(false)
    }
  }

  return (
    <>
      <header class="page-header"><h1>{t('nav.profile')}</h1></header>
      <Show when={props.error}>
        <ErrorState message={errorMessage(props.error, t('common.unexpectedError'))} onRetry={props.refresh} />
      </Show>
      <Show when={props.loading && !props.profile}><LoadingState /></Show>
      <Show when={props.profile}>
        <section class="settings-panel profile-settings-panel" aria-label={t('nav.profile')}>
          <div class="profile-settings-topline">
            <div>
              <h2>{t('profile.identity')}</h2>
              <code class="profile-identity-value">{displayIdentityName(props.profile?.identity_name ?? props.ownerName)}</code>
            </div>
          </div>

          <div class="profile-settings-grid">
            <div class="profile-avatar-editor">
              <Avatar
                name={props.profile?.display_name || displayIdentityName(props.profile?.identity_name ?? props.ownerName)}
                src={avatarUrl()}
                large
              />
              <div class="profile-avatar-controls">
                <div>
                  <span class="profile-control-title">{t('profile.avatar')}</span>
                  <p class="muted">{t('profile.avatarHint')}</p>
                </div>
                <div class="profile-avatar-actions">
                  <label
                    class="button button-secondary profile-file-trigger"
                    classList={{ 'profile-file-trigger-disabled': savingAvatar() }}
                    for="profile-avatar-input"
                    tabindex="0"
                    role="button"
                    onClick={(event) => { if (savingAvatar()) event.preventDefault() }}
                    onKeyDown={(event) => {
                      if (savingAvatar()) return
                      if (event.key === 'Enter' || event.key === ' ') {
                        event.preventDefault()
                        document.getElementById('profile-avatar-input')?.click()
                      }
                    }}
                  >
                    {props.profile?.avatar_url ? t('profile.changeAvatar') : t('profile.uploadAvatar')}
                  </label>
                  <input
                    id="profile-avatar-input"
                    class="sr-only"
                    type="file"
                    aria-label={t('profile.avatar')}
                    accept="image/png,image/jpeg,image/webp"
                    disabled={savingAvatar()}
                    onChange={(event) => {
                      void uploadAvatar(event.currentTarget.files?.[0])
                      event.currentTarget.value = ''
                    }}
                  />
                  <Show when={props.profile?.avatar_url}>
                    <button class="button button-danger-quiet button-small" type="button"
                      disabled={savingAvatar()} onClick={() => void removeAvatar()}>
                      {t('profile.removeAvatar')}
                    </button>
                  </Show>
                </div>
                <Show when={avatarError()}><p class="field-error" role="alert">{avatarError()}</p></Show>
              </div>
            </div>

            <form class="profile-details-form" onSubmit={(event) => { event.preventDefault(); void save() }}>
              <label class="field">
                <span>{t('profile.displayName')}</span>
                <input
                  value={displayName()}
                  aria-invalid={fieldError() !== null}
                  aria-describedby={fieldError() ? 'profile-name-error' : 'profile-name-hint'}
                  onInput={(event) => { setDisplayName(event.currentTarget.value); setFieldError(null) }}
                  onBlur={validate}
                />
              </label>
              <Show when={fieldError()} fallback={<p class="muted" id="profile-name-hint">{t('profile.displayNameHint')}</p>}>
                <p class="field-error" id="profile-name-error" role="alert">{fieldError()}</p>
              </Show>
              <div class="settings-actions">
                <span class="muted">{t('contacts.updated')}: {date(props.profile?.updated_at ?? null)}</span>
                <button class="button button-primary" type="submit" disabled={saving()}>
                  {saving() ? t('common.saving') : t('common.save')}
                </button>
              </div>
            </form>
          </div>
        </section>
      </Show>
    </>
  )
}
