import { For, Show, createEffect, createResource, createSignal, on, onCleanup } from 'solid-js'

import { ApiError, api, request } from '../api/client'
import type { CapabilityDescriptor } from '../api/types'
import { Avatar, Dialog, ErrorState, LoadingState } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { displayIdentityName, errorMessage, normalizeIdentityName, profileAvatarUrl } from '../lib/format'
import ContactsPage from './ContactsPage'

export default function AddContactPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
  navigate: (path: string) => void
  ownerName: string
  initialTarget?: string
}) {
  const { t } = useI18n()
  const load = abortable((_source: boolean, signal: AbortSignal) => api.capabilities(signal))
  const [capabilities, capabilityActions] = createResource(() => true, load)
  const [target, setTarget] = createSignal('')
  const [lookupName, setLookupName] = createSignal<string | null>(null)
  const [recipient, recipientActions] = createResource(() => {
    const name = lookupName()
    return name && name !== normalizeIdentityName(props.ownerName) ? name : false
  }, abortable(async (name: string, signal: AbortSignal) => {
    const [catalog, profile] = await Promise.all([
      request<CapabilityDescriptor[]>(`/std/dhttp/${encodeURIComponent(name)}/std/acl/capabilities`, { signal }),
      api.publicProfile(name, signal),
    ])
    if (!Array.isArray(catalog)) throw new ApiError(502, 'Remote capability catalog unknown or awaiting access approval')
    if ((profile.display_name !== null && typeof profile.display_name !== 'string') || !Number.isFinite(profile.updated_at)) {
      throw new ApiError(502, 'Remote profile unknown or awaiting access approval')
    }
    return { name, catalog, profile }
  }))
  const recipientReady = () => !recipient.loading && !recipient.error && !!lookupName()
    && lookupName() !== normalizeIdentityName(props.ownerName) && recipient()?.name === lookupName()
  const [description, setDescription] = createSignal('')
  const [requestedCapabilities, setRequestedCapabilities] = createSignal<string[]>([])
  const [offeredCapabilities, setOfferedCapabilities] = createSignal<string[]>([])
  const [targetError, setTargetError] = createSignal<string | null>(null)
  const [descriptionError, setDescriptionError] = createSignal<string | null>(null)
  const [submitError, setSubmitError] = createSignal<string | null>(null)
  const [sending, setSending] = createSignal(false)
  let targetInput: HTMLInputElement | undefined
  let descriptionInput: HTMLTextAreaElement | undefined

  createEffect(on(() => props.initialTarget, (value) => {
    const name = normalizeIdentityName(value ?? '')
    setTarget(displayIdentityName(name ?? value ?? ''))
    setLookupName(name)
    setRequestedCapabilities([])
    setOfferedCapabilities([])
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
    if (recipientReady() || recipient.loading) return
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
    if (sending() || !recipientReady() || capabilities.loading || capabilities.error) return
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
        requested_capabilities: requestedCapabilities(),
        offered_capabilities: offeredCapabilities(),
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

  const toggleCapability = (id: string, direction: 'requested' | 'offered') => {
    const setter = direction === 'requested' ? setRequestedCapabilities : setOfferedCapabilities
    setter((current) => current.includes(id)
      ? current.filter((value) => value !== id)
      : [...current, id])
  }

  const recipientLabel = () => recipient.error ? '' : recipient()?.profile.display_name || displayIdentityName(lookupName() ?? '')
  const requestableCapabilities = () => capabilities()?.filter((capability) => capability.selectable) ?? []

  return (
    <>
      <Show when={!recipientReady()}>
        <ContactsPage notify={props.notify} navigate={props.navigate} />
      </Show>
      <Dialog open={!recipientReady()} title={t('outbound.searchTitle')}
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
            <fieldset class="capability-selection">
              <legend>{t('outbound.requestedCapabilities')}</legend>
              <Show when={recipient()?.catalog.filter((capability) => capability.selectable).length === 0}>
                <p class="muted">{t('outbound.noCapabilities')}</p>
              </Show>
              <div class="capability-option-list">
                <For each={recipient()?.catalog.filter((capability) => capability.selectable) ?? []}>{(capability) =>
                  <CapabilityOption capability={capability} checked={requestedCapabilities().includes(capability.id)}
                    direction="requested"
                    onToggle={() => toggleCapability(capability.id, 'requested')} />
                }</For>
              </div>
            </fieldset>
            <fieldset class="capability-selection">
              <legend>{t('outbound.offeredCapabilities')}</legend>
              <Show when={capabilities.loading && !capabilities()}>
                <LoadingState label={t('capabilities.loading')} />
              </Show>
              <Show when={capabilities.error}>
                <ErrorState message={errorMessage(capabilities.error, t('common.unexpectedError'))}
                  onRetry={() => void capabilityActions.refetch()} />
              </Show>
              <Show when={!capabilities.error && capabilities()} fallback={
                <Show when={!capabilities.loading}><p class="muted">{t('outbound.noCapabilities')}</p></Show>
              }>
                <Show when={requestableCapabilities().length === 0}>
                  <p class="muted">{t('outbound.noCapabilities')}</p>
                </Show>
                <div class="capability-option-list">
                  <For each={requestableCapabilities()}>{(capability) =>
                    <CapabilityOption capability={capability} checked={offeredCapabilities().includes(capability.id)}
                      direction="offered"
                      onToggle={() => toggleCapability(capability.id, 'offered')} />
                  }</For>
                </div>
              </Show>
            </fieldset>
            <Show when={submitError()}><p class="field-error" role="alert">{submitError()}</p></Show>
            <div class="settings-actions"><button class="button button-primary" type="submit" disabled={sending() || capabilities.loading || !!capabilities.error}>
              {sending() ? t('outbound.sending') : t('outbound.send')}
            </button></div>
          </form>
        </section>
      </Show>
    </>
  )
}

function CapabilityOption(props: {
  capability: CapabilityDescriptor
  checked: boolean
  direction: 'requested' | 'offered'
  onToggle: () => void
}) {
  const { t } = useI18n()
  const name = () => props.capability.id === 'std.dhttp.net/message#message'
    ? t('capabilities.chat.name') : props.capability.title
  return (
    <label class="capability-option">
      <input type="checkbox" checked={props.checked} onChange={props.onToggle}
        aria-label={`${name()} — ${props.direction === 'requested' ? t('outbound.requestedCapabilities') : t('outbound.offeredCapabilities')}`} />
      <span>
        <strong>{name()}</strong>
        <Show when={props.capability.id === 'std.dhttp.net/message#message'}>
          <small>{t(props.direction === 'requested'
            ? 'outbound.capability.chat.requestSummary'
            : 'outbound.capability.chat.offerSummary')}</small>
        </Show>
      </span>
    </label>
  )
}
