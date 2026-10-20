import { Show, createEffect, createResource, createSignal, on, onCleanup } from 'solid-js'
import type { JSX } from 'solid-js'

import { ApiError, api } from '../api/client'
import { Avatar, Dialog, LoadingState } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { displayIdentityName, errorMessage, normalizeIdentityName, profileAvatarUrl } from '../lib/format'

export default function AddContactPage(props: {
  open: boolean
  children: JSX.Element
  notify: (message: string, tone?: 'success' | 'error') => void
  navigate: (path: string) => void
  ownerName: string
  initialTarget?: string
}) {
  const { t } = useI18n()
  const [target, setTarget] = createSignal('')
  const [lookupName, setLookupName] = createSignal<string | null>(null)
  const [recipient, recipientActions] = createResource(() => {
    const name = lookupName()
    return props.open && name && name !== normalizeIdentityName(props.ownerName) ? name : false
  }, abortable(async (name: string, signal: AbortSignal) => {
    const profile = await api.publicProfile(name, signal).catch(() => ({ display_name: null, avatar_url: null, updated_at: 0 }))
    signal.throwIfAborted()
    return { name, profile }
  }))
  const recipientReady = () => props.open && !recipient.loading && !recipient.error && !!lookupName()
    && lookupName() !== normalizeIdentityName(props.ownerName) && recipient()?.name === lookupName()
  const [description, setDescription] = createSignal('')
  const [targetError, setTargetError] = createSignal<string | null>(null)
  const [descriptionError, setDescriptionError] = createSignal<string | null>(null)
  const [submitError, setSubmitError] = createSignal<string | null>(null)
  const [sending, setSending] = createSignal(false)
  let targetInput: HTMLInputElement | undefined
  let descriptionInput: HTMLTextAreaElement | undefined

  createEffect(on(() => [props.open, props.initialTarget] as const, ([open, value]) => {
    const name = normalizeIdentityName(open ? value ?? '' : '')
    setTarget(displayIdentityName(name ?? value ?? ''))
    setLookupName(name)
    setDescription('')
    setDescriptionError(null)
    setTargetError(null)
    setSubmitError(null)
  }))

  createEffect(() => {
    if (!recipientReady()) return
    const name = lookupName()!
    if (normalizeIdentityName(props.initialTarget ?? '') !== name) {
      props.navigate(`#/contacts/new?${new URLSearchParams({ target: name })}`)
    }
  })

  createEffect(() => {
    if (!props.open || recipientReady() || recipient.loading) return
    const frame = window.requestAnimationFrame(() => targetInput?.focus())
    onCleanup(() => window.cancelAnimationFrame(frame))
  })

  const targetValidationError = (): string | null => {
    const name = normalizeIdentityName(target())
    if (!name) return t('outbound.invalidTarget')
    if (name === normalizeIdentityName(props.ownerName)) return t('outbound.ownIdentity')
    return null
  }
  const validDescription = () => [...description().trim()].length <= 80 && !/\p{Cc}/u.test(description())

  const search = () => {
    if (recipient.loading) return
    const message = targetValidationError()
    setTargetError(message)
    if (message) {
      targetInput?.focus()
      return
    }
    const name = normalizeIdentityName(target())!
    if (lookupName() === name) void recipientActions.refetch()
    else setLookupName(name)
  }

  const send = async () => {
    if (sending() || !recipientReady()) return
    const targetName = lookupName()
    const descriptionIsValid = validDescription()
    setDescriptionError(descriptionIsValid ? null : t('outbound.invalidDescription'))
    if (!targetName || !descriptionIsValid) {
      descriptionInput?.focus()
      return
    }
    setSending(true)
    setSubmitError(null)
    try {
      await api.sendContactRequest({
        target_name: targetName,
        description: description().trim(),
        requested_capabilities: [],
        offered_capabilities: [],
      })
      props.notify(t('outbound.sent'))
      props.navigate('#/contacts/requests')
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) setSubmitError(t('outbound.duplicate'))
      else if (error instanceof ApiError && error.status === 400 && error.message.includes('own identity')) {
        setSubmitError(t('outbound.ownIdentity'))
      } else if (error instanceof ApiError && error.status === 400 && error.message.includes('target name')) {
        setSubmitError(t('outbound.invalidTarget'))
      } else setSubmitError(errorMessage(error, t('common.unexpectedError')))
    } finally {
      setSending(false)
    }
  }

  const recipientLabel = () => recipient.error ? '' : recipient()?.profile.display_name || displayIdentityName(lookupName() ?? '')

  return (
    <>
      <div hidden={recipientReady()}>{props.children}</div>
      <Dialog open={props.open && !recipientReady()} title={t('outbound.searchTitle')}
        onClose={() => props.navigate('#/contacts')}>
        <form class="contact-search-form" onSubmit={(event) => { event.preventDefault(); search() }}>
          <label class="field">
            <input ref={targetInput} value={target()} aria-label={t('outbound.target')}
              aria-invalid={targetError() !== null} aria-required="true"
              disabled={recipient.loading} autocomplete="off"
              onInput={(event) => { setTarget(event.currentTarget.value); setTargetError(null) }} />
            <Show when={targetError()}><span class="field-error" role="alert">{targetError()}</span></Show>
          </label>
          <Show when={recipient.loading}><LoadingState label={t('outbound.searching')} /></Show>
          <Show when={recipient.error}>
            <div class="contact-search-error" role="alert">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
                <path d="M12 3 2 21h20L12 3Z" stroke-linejoin="round" />
                <path d="M12 9v5m0 3v1" />
              </svg>
              <div>
                <p>{t('outbound.searchFailed')}</p>
                <small>{errorMessage(recipient.error, t('common.unexpectedError'))}</small>
              </div>
            </div>
          </Show>
          <div class="dialog-actions">
            <button class="button button-secondary" type="button" onClick={() => props.navigate('#/contacts')}>{t('common.cancel')}</button>
            <button class="button button-primary" type="submit" disabled={recipient.loading}>
              {recipient.loading ? t('outbound.searching') : t('outbound.search')}
            </button>
          </div>
        </form>
      </Dialog>
      <Show when={recipientReady()}>
        <header class="page-header contacts-page-header contact-new-header">
          <h1>{t('nav.contactNew')}</h1>
          <div class="header-actions">
            <button class="text-link" type="button"
              onClick={() => props.navigate('#/contacts/requests')}>
              {t('nav.contactRequests')}
            </button>
          </div>
        </header>
        <section class="settings-panel outbound-form" aria-label={t('nav.contactNew')}>
          <form onSubmit={(event) => { event.preventDefault(); void send() }}>
            <div class="contact-heading contact-request-recipient">
              <Avatar name={recipientLabel()} src={profileAvatarUrl(recipient()?.profile)} large />
              <div class="contact-identity-detail">
                <strong>{recipientLabel()}</strong>
                <small>{lookupName()}</small>
              </div>
              <button class="button button-secondary" type="button" disabled={sending()}
                onClick={() => props.navigate('#/contacts/new')}>{t('outbound.changeRecipient')}</button>
            </div>
            <label class="field"><span>{t('outbound.description')}</span>
              <textarea ref={descriptionInput} value={description()} rows={2} aria-invalid={descriptionError() !== null}
                onInput={(event) => { setDescription(event.currentTarget.value); setDescriptionError(null) }}
                onBlur={() => setDescriptionError(validDescription() ? null : t('outbound.invalidDescription'))} />
              <Show when={descriptionError()}><span class="field-error" role="alert">{descriptionError()}</span></Show>
            </label>
            <Show when={submitError()}><p class="field-error" role="alert">{submitError()}</p></Show>
            <div class="settings-actions"><button class="button button-primary" type="submit" disabled={sending()}>
              {sending() ? t('outbound.sending') : t('outbound.send')}
            </button></div>
          </form>
        </section>
      </Show>
    </>
  )
}
