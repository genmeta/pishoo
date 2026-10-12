import { For, Show, createMemo, createResource, createSignal } from 'solid-js'

import { api } from '../api/client'
import type { Contact } from '../api/types'
import { contactClassLabel, useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { contactDisplayName, contactStatus, displayIdentityName, initials } from '../lib/format'

function contactLabel(contact: Contact): string {
  return contactDisplayName(contact.name, contact.alias?.trim())
}

export default function WorkspaceChatBox() {
  const { t } = useI18n()
  const [draft, setDraft] = createSignal('')
  const [cursor, setCursor] = createSignal(0)
  const [mentionIndex, setMentionIndex] = createSignal(0)
  const [mentionDismissed, setMentionDismissed] = createSignal(false)
  let textarea: HTMLTextAreaElement | undefined

  const loadContacts = abortable((_source: boolean, signal: AbortSignal) =>
    api.allContacts('name', 'asc', signal),
  )
  const [contacts] = createResource(() => true, loadContacts)

  const mention = createMemo(() => {
    if (mentionDismissed()) return null
    const beforeCursor = draft().slice(0, cursor())
    const match = /(?:^|\s)@([^\s@]*)$/.exec(beforeCursor)
    if (!match) return null
    const atOffset = match.index + match[0].indexOf('@')
    return {
      start: atOffset,
      end: cursor(),
      query: match[1],
    }
  })

  const availableContacts = createMemo(() => (contacts() ?? [])
    .filter((contact) => contactStatus(contact.status) === 'active'))

  const suggestions = createMemo(() => {
    const currentMention = mention()
    if (!currentMention) return []
    const query = currentMention.query.toLowerCase()
    return availableContacts()
      .filter((contact) => {
        const searchText = `${contactLabel(contact)} ${displayIdentityName(contact.name)} ${contact.class}`
        return searchText.toLowerCase().includes(query)
      })
      .slice(0, 8)
  })

  const updateCursor = (event: Event) => {
    const target = event.currentTarget as HTMLTextAreaElement
    setCursor(target.selectionStart ?? target.value.length)
  }

  const onInput = (event: InputEvent) => {
    const target = event.currentTarget as HTMLTextAreaElement
    setDraft(target.value)
    setCursor(target.selectionStart ?? target.value.length)
    setMentionDismissed(false)
    setMentionIndex(0)
  }

  const selectMention = (contact: Contact) => {
    const currentMention = mention()
    if (!currentMention) return
    const insertion = `@${contactLabel(contact)} `
    const nextDraft = draft().slice(0, currentMention.start)
      + insertion
      + draft().slice(currentMention.end)
    const nextCursor = currentMention.start + insertion.length
    setDraft(nextDraft)
    setCursor(nextCursor)
    setMentionIndex(0)
    queueMicrotask(() => {
      textarea?.focus()
      textarea?.setSelectionRange(nextCursor, nextCursor)
    })
  }

  const onKeyDown = (event: KeyboardEvent) => {
    const items = suggestions()
    if (event.key === 'Escape' && mention()) {
      event.preventDefault()
      setMentionDismissed(true)
      return
    }
    if (!mention() || !items.length) return
    if (event.key === 'ArrowDown') {
      event.preventDefault()
      setMentionIndex((current) => (current + 1) % items.length)
    } else if (event.key === 'ArrowUp') {
      event.preventDefault()
      setMentionIndex((current) => (current - 1 + items.length) % items.length)
    } else if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault()
      selectMention(items[mentionIndex()] ?? items[0])
    }
  }

  return (
    <aside class="quick-chat-dock" aria-label={t('quickChat.title')}>
      <section class="quick-chat-shell">
        <header class="quick-chat-header">
          <h2>{t('quickChat.title')}</h2>
        </header>

        <form class="quick-chat-form" onSubmit={(event) => event.preventDefault()}>
          <div class="quick-chat-input-wrap">
            <label class="sr-only" for="workspace-quick-chat-input">{t('quickChat.messageLabel')}</label>
            <textarea
              id="workspace-quick-chat-input"
              ref={textarea}
              value={draft()}
              rows={2}
              maxLength={4000}
              placeholder={t('quickChat.placeholder')}
              aria-autocomplete="list"
              aria-controls="workspace-quick-chat-mentions"
              aria-expanded={mention() !== null}
              onInput={onInput}
              onKeyDown={onKeyDown}
              onKeyUp={updateCursor}
              onClick={updateCursor}
            />

            <Show when={mention()} keyed>
              {(currentMention) => (
                <div
                  id="workspace-quick-chat-mentions"
                  class="quick-chat-mentions"
                  role="listbox"
                  aria-label={t('quickChat.mentionContacts')}
                >
                  <Show when={!contacts.loading && !contacts.error && suggestions().length > 0} fallback={
                    <p class="quick-chat-mentions-empty">
                      {contacts.loading ? t('quickChat.loadingContacts')
                        : contacts.error ? t('quickChat.contactsUnavailable')
                          : t('quickChat.noContacts')}
                    </p>
                  }>
                    <For each={suggestions()}>{(contact, index) => (
                      <button
                        type="button"
                        class="quick-chat-mention"
                        classList={{ active: mentionIndex() === index() }}
                        role="option"
                        aria-selected={mentionIndex() === index()}
                        onMouseDown={(event) => {
                          event.preventDefault()
                          selectMention(contact)
                        }}
                        onMouseEnter={() => setMentionIndex(index())}
                      >
                        <span class="quick-chat-mention-avatar" aria-hidden="true">
                          {initials(contactLabel(contact))}
                        </span>
                        <span class="quick-chat-mention-copy">
                          <strong>{contactLabel(contact)}</strong>
                          <small>@{displayIdentityName(contact.name)}</small>
                        </span>
                        <span class="quick-chat-mention-kind">{contactClassLabel(t, contact.class)}</span>
                      </button>
                    )}</For>
                  </Show>
                  <span class="sr-only">{currentMention.query}</span>
                </div>
              )}
            </Show>
          </div>
          <div class="quick-chat-actions">
            <button class="button button-primary" type="submit" disabled title={t('quickChat.comingSoon')}>
              {t('quickChat.send')}
            </button>
          </div>
        </form>
      </section>
    </aside>
  )
}
