import { For, Show, createResource, createSignal } from 'solid-js'

import { ApiError, api } from '../api/client'
import type { CapabilityDescriptor } from '../api/types'
import { ErrorState, LoadingState } from '../components/Ui'
import { capabilityLabel, useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { errorMessage } from '../lib/format'

export default function AddContactPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
  navigate: (path: string) => void
}) {
  const { t } = useI18n()
  const load = abortable((_source: boolean, signal: AbortSignal) => api.capabilities(signal))
  const [capabilities, capabilityActions] = createResource(() => true, load)
  const [target, setTarget] = createSignal('')
  const [description, setDescription] = createSignal('')
  const [requestedCapabilities, setRequestedCapabilities] = createSignal<string[]>([])
  const [offeredCapabilities, setOfferedCapabilities] = createSignal<string[]>([])
  const [targetError, setTargetError] = createSignal<string | null>(null)
  const [descriptionError, setDescriptionError] = createSignal<string | null>(null)
  const [submitError, setSubmitError] = createSignal<string | null>(null)
  const [sending, setSending] = createSignal(false)
  let targetInput: HTMLInputElement | undefined
  let descriptionInput: HTMLTextAreaElement | undefined

  const validTarget = () => target().trim().length > 0 && !/[\s/?#:@]/u.test(target())
  const validDescription = () => [...description().trim()].length <= 80 && !/\p{Cc}/u.test(description())

  const send = async () => {
    if (sending()) return
    const targetIsValid = validTarget()
    const descriptionIsValid = validDescription()
    setTargetError(targetIsValid ? null : t('outbound.invalidTarget'))
    setDescriptionError(descriptionIsValid ? null : t('outbound.invalidDescription'))
    if (!targetIsValid || !descriptionIsValid) {
      if (!targetIsValid) targetInput?.focus()
      else if (!descriptionIsValid) descriptionInput?.focus()
      return
    }
    setSending(true)
    setSubmitError(null)
    try {
      await api.sendContactRequest({
        target_name: target().trim(),
        description: description().trim(),
        requested_capabilities: requestedCapabilities(),
        offered_capabilities: offeredCapabilities(),
      })
      props.notify(t('outbound.sent'))
      props.navigate('/std/workspace/contacts/requests')
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) setTargetError(t('outbound.duplicate'))
      else if (error instanceof ApiError && error.status === 400 && error.message.includes('target name')) {
        setTargetError(t('outbound.invalidTarget'))
        targetInput?.focus()
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

  const requestableCapabilities = () => capabilities()?.filter((capability) => capability.selectable) ?? []

  return (
    <>
      <header class="page-header contacts-page-header contact-new-header">
        <h1>{t('nav.contactNew')}</h1>
        <div class="header-actions">
          <button class="button button-secondary" type="button"
            onClick={() => props.navigate('/std/workspace/contacts/requests')}>
            {t('nav.contactRequests')}
          </button>
        </div>
      </header>
      <section class="settings-panel outbound-form" aria-label={t('nav.contactNew')}>
        <form onSubmit={(event) => { event.preventDefault(); void send() }}>
          <label class="field"><span>{t('outbound.target')}</span>
            <input ref={targetInput} value={target()} aria-invalid={targetError() !== null} aria-required="true"
              onInput={(event) => { setTarget(event.currentTarget.value); setTargetError(null) }}
              onBlur={() => setTargetError(validTarget() ? null : t('outbound.invalidTarget'))} />
            <Show when={targetError()}><span class="field-error" role="alert">{targetError()}</span></Show>
          </label>
          <label class="field"><span>{t('outbound.description')}</span>
            <textarea ref={descriptionInput} value={description()} rows={2} aria-invalid={descriptionError() !== null}
              onInput={(event) => { setDescription(event.currentTarget.value); setDescriptionError(null) }}
              onBlur={() => setDescriptionError(validDescription() ? null : t('outbound.invalidDescription'))} />
            <Show when={descriptionError()}><span class="field-error" role="alert">{descriptionError()}</span></Show>
          </label>
          <fieldset class="capability-selection">
            <legend>{t('outbound.requestedCapabilities')}</legend>
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
              <div class="capability-option-list">
                <For each={requestableCapabilities()}>{(capability) =>
                  <CapabilityOption capability={capability} checked={requestedCapabilities().includes(capability.id)}
                    direction="requested"
                    onToggle={() => toggleCapability(capability.id, 'requested')} />
                }</For>
              </div>
            </Show>
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
          <div class="settings-actions"><button class="button button-primary" type="submit" disabled={sending()}>
            {sending() ? t('outbound.sending') : t('outbound.send')}
          </button></div>
        </form>
      </section>
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
  const name = () => capabilityLabel(t, props.capability.id)
  const summary = () => {
    if (props.capability.id !== 'chat') return ''
    return t(props.direction === 'requested'
      ? 'outbound.capability.chat.requestSummary'
      : 'outbound.capability.chat.offerSummary')
  }
  return (
    <label class="capability-option">
      <input type="checkbox" checked={props.checked} onChange={props.onToggle}
        aria-label={`${name()} — ${props.direction === 'requested' ? t('outbound.requestedCapabilities') : t('outbound.offeredCapabilities')}`} />
      <span>
        <strong>{name()}</strong>
        <small>{summary()}</small>
      </span>
    </label>
  )
}
