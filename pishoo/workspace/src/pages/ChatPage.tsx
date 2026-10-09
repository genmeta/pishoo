import { For, Show, createResource, createSignal, onCleanup, onMount } from 'solid-js'

import { api } from '../api/client'
import type { ChatMessage } from '../api/types'
import { chatApi } from '../chat/api'
import { Avatar, EmptyState, ErrorState, LoadingState, StatusBadge } from '../components/Ui'
import { contactFollowupLabel, statusLabel, useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { contactDisplayName, contactStatus, displayIdentityName, errorMessage, profileAvatarUrl } from '../lib/format'
import { contactPath } from '../lib/routes'

function deliveryReason(message: ChatMessage): string | null {
  return message.state === 'failed' || message.state === 'blocked'
    ? message.error_message
    : null
}

function DeliveryStatus(props: { message: ChatMessage }) {
  const { t } = useI18n()
  const reason = deliveryReason(props.message)
  const status = () => statusLabel(t, props.message.state)
  return (
    <span class="chat-delivery"
      classList={{
        'chat-delivery-sent': props.message.state === 'sent' || props.message.state === 'received',
        'chat-delivery-failed': props.message.state === 'failed' || props.message.state === 'blocked',
      }}
      role="img"
      aria-label={reason ? t('chat.deliveryIssue', { status: status(), reason }) : status()}
      title={reason ?? status()}
      tabIndex={reason ? 0 : undefined}>
      <svg viewBox="0 0 20 20" aria-hidden="true">
        <Show when={props.message.state === 'sent' || props.message.state === 'received'}>
          <path d="m4 10 4 4 8-8" />
        </Show>
        <Show when={props.message.state === 'failed' || props.message.state === 'blocked'}>
          <path d="M5 5 15 15M15 5 5 15" />
        </Show>
        <Show when={props.message.state === 'queued' || props.message.state === 'sending'}>
          <circle cx="10" cy="10" r="7" />
          <path d="M10 6v4l3 2" />
        </Show>
      </svg>
    </span>
  )
}

export default function ChatPage(props: {
  contactName: string
  navigate: (path: string) => void
  notify: (message: string, tone?: 'success' | 'error') => void
}) {
  const { date, t } = useI18n()
  const [draft, setDraft] = createSignal('')
  const [submitting, setSubmitting] = createSignal(false)
  const [actionError, setActionError] = createSignal<string | null>(null)
  let feed: HTMLDivElement | undefined

  const loadContact = abortable((name: string, signal: AbortSignal) => api.contact(name, signal))
  const [contact, contactActions] = createResource(() => props.contactName, loadContact)
  const loadProfile = abortable((name: string, signal: AbortSignal) => api.publicProfile(name, signal))
  const [profile] = createResource(() => contact()?.name ?? null, loadProfile)
  const loadCapability = abortable((name: string, signal: AbortSignal) => chatApi.capability(name, signal))
  const [capability, capabilityActions] = createResource(() => contact()?.name ?? null, loadCapability)
  const canChat = () => !capability.error && capability()?.status === 'available'
  const canSend = () => !capability.error && capability()?.can_send === true
  let loadedName: string | undefined
  let loadedMessages: ChatMessage[] = []
  const loadMessages = abortable(async (name: string, signal: AbortSignal) => {
    const previous = loadedName === name ? loadedMessages : []
    // Delivered messages are immutable. Refresh from the first unresolved
    // message so retries and permission changes also update older messages.
    const unresolved = previous.findIndex((message) => !['sent', 'received'].includes(message.state))
    const items = unresolved === -1 ? [...previous] : previous.slice(0, unresolved)
    let cursor: string | null = items.at(-1)?.id ?? null
    do {
      const page = await chatApi.messages(name, cursor, 100, signal)
      items.push(...page.items)
      cursor = page.next_cursor
    } while (cursor)
    signal.throwIfAborted()
    loadedName = name
    loadedMessages = items
    const scrollTop = feed?.scrollTop ?? 0
    const followMessages = !feed?.isConnected || feed.scrollHeight - feed.clientHeight - scrollTop < 32
    window.requestAnimationFrame(() => {
      if (feed?.isConnected) feed.scrollTop = followMessages ? feed.scrollHeight : scrollTop
    })
    return { items, next_cursor: null }
  })
  const [messages, messageActions] = createResource(() => canChat() ? props.contactName : null, loadMessages)

  onMount(() => {
    const refresh = () => {
      if (document.hidden) return
      if (contact() && !capability.loading) void capabilityActions.refetch()
      if (canChat() && !messages.loading) void messageActions.refetch()
    }
    const timer = window.setInterval(refresh, 3000)
    document.addEventListener('visibilitychange', refresh)
    onCleanup(() => {
      window.clearInterval(timer)
      document.removeEventListener('visibilitychange', refresh)
    })
  })

  const resolvedProfile = () => profile.error ? undefined : profile()
  const label = () => contactDisplayName(props.contactName, contact()?.alias, resolvedProfile()?.display_name)
  const identityName = () => displayIdentityName(props.contactName)
  const avatarUrl = () => profileAvatarUrl(resolvedProfile())

  const back = () => props.navigate(contactPath(props.contactName))

  const send = async (event: SubmitEvent) => {
    event.preventDefault()
    const text = draft().trim()
    if (!text || submitting() || !canChat() || !canSend()) return
    setSubmitting(true)
    setActionError(null)
    try {
      await chatApi.sendMessage(props.contactName, text)
      setDraft('')
      await messageActions.refetch()
      props.notify(t('chat.queued'))
    } catch (error) {
      setActionError(errorMessage(error, t('chat.sendFailed')))
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <div class="chat-page">
      <header class="page-header chat-page-header">
        <div class="chat-title-group">
          <button class="icon-button" type="button" aria-label={t('chat.back')} title={t('chat.back')} onClick={back}>‹</button>
          <Avatar name={label()} src={avatarUrl()} large />
          <div>
            <h1>{label()}</h1>
            <p class="chat-identity">{identityName()}</p>
          </div>
        </div>
        <div class="chat-toolbar">
          <Show when={contact()}>
            <StatusBadge value={contactStatus(contact()!.status)} />
          </Show>
        </div>
      </header>

      <Show when={contact.error}>
        <ErrorState message={errorMessage(contact.error, t('common.unexpectedError'))} onRetry={() => void contactActions.refetch()} />
      </Show>
      <Show when={contact.loading && !contact()}>
        <LoadingState label={t('chat.loadingContact')} />
      </Show>
      <Show when={contact() && capability.error}>
        <ErrorState
          message={errorMessage(capability.error, t('chat.capabilityLoadFailed'))}
          onRetry={() => void capabilityActions.refetch()}
        />
      </Show>
      <Show when={contact() && capability.loading && !capability()}>
        <LoadingState label={t('chat.loadingCapability')} />
      </Show>
      <Show when={contact() && !capability.loading && !capability.error && !canChat()}>
        <section class="chat-state-panel" role="status">
          <StatusBadge value={contactStatus(contact()!.status)} />
          <h2>{t('chat.unavailableTitle')}</h2>
          <p>{t(capability()?.status === 'blocked' ? 'chat.blockedDetail' : 'chat.waitingDetail')}</p>
          <button class="button button-secondary" type="button" onClick={back}>{t('chat.backToContact')}</button>
        </section>
      </Show>

      <Show when={contact() && canChat()}>
        <section class="chat-workspace" aria-label={t('chat.conversation')}>
          <div class="chat-feed" ref={feed} aria-live="polite">
            <Show when={messages.error}>
              <ErrorState
                message={errorMessage(messages.error, t('chat.loadFailed'))}
                onRetry={() => void messageActions.refetch()}
              />
            </Show>
            <Show when={messages.loading && !messages()}>
              <LoadingState label={t('chat.loadingMessages')} />
            </Show>
            <Show when={!messages.error && messages() && messages()!.items.length === 0}>
              <EmptyState title={t('chat.empty')} />
            </Show>
            <Show when={!messages.error && messages()?.items.length}>
              <For each={messages()?.items}>
                {(message) => (
                  <article class="chat-message"
                    classList={{ 'chat-message-outgoing': message.direction === 'outgoing' }}
                    aria-label={t(message.direction === 'outgoing' ? 'chat.outgoingMessage' : 'chat.incomingMessage')}
                    title={date(message.created_at)}>
                    <Show when={message.direction === 'outgoing'}>
                      <DeliveryStatus message={message} />
                    </Show>
                    <div class="chat-message-bubble">
                      <p>{message.text}</p>
                    </div>
                  </article>
                )}
              </For>
            </Show>
          </div>

          <Show when={actionError()}>
            <p class="field-error chat-action-error" role="alert">{actionError()}</p>
          </Show>
          <form class="chat-composer" onSubmit={send}>
            <Show when={canSend() && capability()?.remote_grant !== true}>
              <p class="muted">{contactFollowupLabel(t, capability()?.remote_grant === false
                ? 'remote-not-granted' : 'remote-unknown')}</p>
            </Show>
            <Show when={!canSend()}><p class="muted">{t('chat.sendUnavailable')}</p></Show>
            <label class="field">
              <span>{t('chat.messageLabel')}</span>
              <textarea value={draft()} maxLength={4000} rows={3} disabled={submitting() || !canSend()}
                placeholder={t('chat.messagePlaceholder')}
                onInput={(event) => setDraft(event.currentTarget.value)} />
            </label>
            <button class="button button-primary" type="submit" disabled={!draft().trim() || submitting() || !canSend()}>
              {submitting() ? t('chat.sending') : t('chat.send')}
            </button>
          </form>
        </section>
      </Show>
    </div>
  )
}
