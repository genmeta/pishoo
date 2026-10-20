import { For, Show, createEffect, createMemo, createResource, createSignal, onCleanup, onMount } from 'solid-js'

import { api } from '../api/client'
import type { Contact, DirectoryEntry } from '../api/types'
import { chatApi } from '../chat/api'
import { Avatar, Dialog, EmptyState, ErrorState, LoadingState, Pagination, StatusBadge } from '../components/Ui'
import { contactClassLabel, useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { contactDisplayName, contactStatus, profileAvatarUrl, displayIdentityName, errorMessage } from '../lib/format'
import { contactChatPath, contactPath } from '../lib/routes'
import { useContactDirectoryProfiles } from '../lib/useContactDirectoryProfiles'

type ConfirmDelete = { names: string[] } | null
function ContactIdentity(props: { contact: Contact; large?: boolean; detail?: boolean; entry?: DirectoryEntry }) {
  const resolvedProfile = () => {
    if (props.entry) return { display_name: props.entry.display_name, avatar_url: props.entry.avatar_url, updated_at: props.entry.profile_updated_at }
  }
  const identityName = () => displayIdentityName(props.contact.name)
  const label = () => contactDisplayName(props.contact.name, props.contact.alias, resolvedProfile()?.display_name)
  return (
    <>
      <Avatar name={label()} src={profileAvatarUrl(resolvedProfile())} large={props.large} />
      <span classList={{ 'contact-identity-detail': props.detail }}>
        <strong>{label()}</strong>
        <Show when={identityName() !== label()}><small>{identityName()}</small></Show>
      </span>
    </>
  )
}

export default function ContactsPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
  contactName?: string
  navigate: (path: string) => void
  returnTo?: string
  pausePolling?: boolean
  onResolved?: () => void
}) {
  const { date, t } = useI18n()
  const [page, setPage] = createSignal(1)
  const [search, setSearch] = createSignal('')
  const [revision, setRevision] = createSignal(0)
  const selectedName = () => props.contactName ?? null
  const openContact = (name: string) => props.navigate(contactPath(name))
  const openChat = (name: string) => props.navigate(contactChatPath(name))
  const closeContact = () => props.navigate(props.returnTo ?? '#/contacts')
  const [confirmDelete, setConfirmDelete] = createSignal<ConfirmDelete>(null)
  const [alias, setAlias] = createSignal('')
  const [submitting, setSubmitting] = createSignal(false)
  const loadContacts = abortable((_revision: number, signal: AbortSignal) => api.contactDirectory(signal))
  const loadContact = abortable((name: string, signal: AbortSignal) => api.contact(name, signal))
  const [contacts] = createResource(revision, loadContacts)
  const [detail, detailActions] = createResource(selectedName, loadContact)
  const directory = useContactDirectoryProfiles(contacts)
  const directoryEntries = createMemo(() => new Map<string, DirectoryEntry>(
    directory().map(entry => [entry.name, entry]),
  ))
  const chatAvailable = createMemo(() => new Set(
    directory().filter(entry => entry.chat_available).map(entry => entry.name),
  ))
  const filtered = () => directory().filter(contact => {
    const text = `${contactDisplayName(contact.name, contact.alias, contact.display_name)} ${contact.name}`
    return text.toLocaleLowerCase().includes(search().trim().toLocaleLowerCase())
  })
  const visible = () => filtered().slice((page() - 1) * 20, page() * 20)

  const refresh = () => setRevision((value) => value + 1)
  onMount(() => {
    const timer = window.setInterval(() => {
      if (document.hidden || submitting() || props.pausePolling) return
      if (selectedName()) return
      if (!contacts.loading && !contacts.error
        && contacts()?.some(contact => contactStatus(contact.status) === 'pending')) refresh()
    }, 3000)
    onCleanup(() => window.clearInterval(timer))
  })
  createEffect(() => {
    if (!contacts()) return
    const lastPage = Math.max(1, Math.ceil(filtered().length / 20))
    if (page() > lastPage) setPage(lastPage)
  })
  createEffect(() => {
    setAlias(detail()?.alias ?? '')
  })
  const mutateContact = async (body: { status?: string; alias?: string }, success: string) => {
    const name = selectedName()
    if (!name) return
    setSubmitting(true)
    try {
      await api.patchContact(name, body)
      props.notify(success)
      refresh()
      props.onResolved?.()
      await detailActions.refetch()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setSubmitting(false)
    }
  }

  const grantChat = () => {
    const name = selectedName()
    if (!name || submitting()) return
    setSubmitting(true)
    void api.capabilities().then((catalog) => {
      const descriptor = catalog.find((c) => c.endpoints.some((e) => e.method === 'POST' && e.path === '/std/message'))
      if (!descriptor) throw new Error('Message capability unavailable')
      return chatApi.grantCapability(name, descriptor.id)
    }).then(async () => {
      props.notify(t('contacts.chatGranted'))
      refresh()
      props.onResolved?.()
      await detailActions.refetch()
    }).catch((error) => {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    }).finally(() => setSubmitting(false))
  }

  const revokeChat = () => {
    const name = selectedName()
    if (!name || submitting()) return
    setSubmitting(true)
    void api.capabilities().then((catalog) => {
      const descriptor = catalog.find((c) => c.endpoints.some((e) => e.method === 'POST' && e.path === '/std/message'))
      if (!descriptor) throw new Error('Message capability unavailable')
      return chatApi.revokeCapability(name, descriptor.id)
    }).then(async () => {
      props.notify(t('contacts.chatRevoked'))
      refresh()
      props.onResolved?.()
      await detailActions.refetch()
    }).catch((error) => {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    }).finally(() => setSubmitting(false))
  }

  const removeContacts = async () => {
    const names = confirmDelete()?.names ?? []
    if (!names.length) return
    setSubmitting(true)
    try {
      if (names.length === 1) await api.deleteContact(names[0])
      else await api.deleteContacts(names)
      props.notify(
        names.length === 1
          ? t('contacts.deletedOne')
          : t('contacts.deletedMany', { count: names.length }),
      )
      if (selectedName() && names.includes(selectedName()!)) closeContact()
      setConfirmDelete(null)
      refresh()
      props.onResolved?.()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setSubmitting(false)
    }
  }

  const statusActions = (contact: Contact) => {
    const status = contactStatus(contact.status)
    return {
      canAccept: ['pending', 'transfered'].includes(status),
      canGrantChat: ['pending', 'transfered', 'active'].includes(status)
        && !detail()?.granted_access['/std/message']?.allow.includes('POST'),
      canRevokeChat: status === 'active' && detail()?.granted_access['/std/message']?.allow.includes('POST') === true,
      canBlock: ['active', 'transfered'].includes(status),
      canRestore: status === 'blocked',
      status,
    }
  }

  return (
    <>
      <header class="page-header contacts-page-header">
        <h1>{t('contacts.title')}</h1>
        <div class="header-actions">
          <button class="button button-primary" type="button"
            onClick={() => props.navigate('#/contacts/new')}>
            {t('nav.contactNew')}
          </button>
          <button class="button button-secondary" type="button" onClick={refresh} disabled={contacts.loading}>
            {t('common.refresh')}
          </button>
        </div>
      </header>

      <div class="toolbar toolbar-wrap">
        <span class="toolbar-meta">{t('contacts.count', { count: filtered().length })}</span>
        <label class="contact-search">
          <span class="sr-only">{t('contacts.search')}</span>
          <input type="search" placeholder={t('contacts.search')} value={search()} onInput={event => {
            setSearch(event.currentTarget.value); setPage(1)
          }} />
        </label>
      </div>

      <section class="data-panel" aria-label={t('contacts.section')}>
        <Show when={contacts.error}>
          <ErrorState message={errorMessage(contacts.error, t('common.unexpectedError'))} onRetry={refresh} />
        </Show>
        <Show when={contacts.loading && !contacts()}>
          <LoadingState label={t('contacts.loading')} />
        </Show>
        <Show when={!contacts.error && contacts() && filtered().length === 0}>
          <EmptyState title={t('contacts.empty')} />
        </Show>
        <Show when={!contacts.error && filtered().length}>
          <div class="contact-card-grid" role="list">
            <For each={visible()}>{contact => (
              <article class="contact-card" role="listitem" classList={{ selected: selectedName() === contact.name }}>
                <div class="contact-card-heading">
                  <button class="contact-link" type="button" onClick={() => openContact(contact.name)}>
                    <ContactIdentity contact={contact} entry={directoryEntries().get(contact.name)} large />
                  </button>
                </div>
                <div class="contact-row-status">
                  <StatusBadge value={contactStatus(contact.status)} />
                  <span class="muted">{contactClassLabel(t, contact.class)}</span>

                </div>
                <div class="contact-card-actions">
                  <Show when={chatAvailable().has(contact.name)}>
                    <button class="button button-secondary" type="button"
                      aria-label={t('contacts.chatWith', { name: displayIdentityName(contact.name) })}
                      onClick={() => openChat(contact.name)}>{t('contacts.openChat')}</button>
                  </Show>
                  <button class="button button-secondary" type="button"
                    aria-label={t('contacts.openContact', { name: displayIdentityName(contact.name) })}
                    onClick={() => openContact(contact.name)}>{t('contacts.openDetails')}</button>
                </div>
              </article>
            )}</For>
          </div>
          <Pagination
            page={page()}
            pageSize={20}
            total={filtered().length}
            onPage={setPage}
          />
        </Show>
      </section>
      <Show when={selectedName()}>
        <div class="drawer-backdrop" onMouseDown={(event) => event.target === event.currentTarget && closeContact()}>
          <aside class="detail-drawer" aria-label={t('contacts.details')}>
            <header class="drawer-header">
              <h2>{contactDisplayName(detail()?.name ?? selectedName() ?? '', detail()?.alias)}</h2>
              <button class="icon-button" type="button" aria-label={t('contacts.closeDetails')} onClick={closeContact}>
                ×
              </button>
            </header>

            <Show when={detail.loading}><LoadingState label={t('contacts.loadingOne')} /></Show>
            <Show when={detail.error}>
              <ErrorState
                message={errorMessage(detail.error, t('common.unexpectedError'))}
                onRetry={() => detailActions.refetch()}
              />
            </Show>
            <Show when={detail()} keyed>
              {(contact) => {
                const actions = () => statusActions(contact)
                return (
                  <div class="drawer-content">
                    <div class="contact-heading">
                      <ContactIdentity contact={contact} entry={directoryEntries().get(contact.name)} large detail />
                      <StatusBadge value={actions().status} />
                    </div>

                    <dl class="metadata-grid">
                      <Show when={contact.description}>
                        <div class="wide"><dt>{t('contacts.description')}</dt><dd>{contact.description}</dd></div>
                      </Show>
                      <div><dt>{t('contacts.alias')}</dt><dd>{contact.alias || t('common.none')}</dd></div>
                      <div><dt>{t('contacts.class')}</dt><dd>{contactClassLabel(t, contact.class)}</dd></div>
                      <div><dt>{t('contacts.updated')}</dt><dd>{date(contact.updated_at)}</dd></div>
                      <div><dt>{t('contacts.created')}</dt><dd>{date(contact.created_at)}</dd></div>
                      <div><dt>{t('contacts.expires')}</dt><dd>{date(contact.valid_before)}</dd></div>
                      <div class="wide"><dt>{t('contacts.subjectId')}</dt><dd><code>{contact.subject_id}</code></dd></div>
                    </dl>

                    <form
                      class="inline-form"
                      onSubmit={(event) => {
                        event.preventDefault()
                        void mutateContact({ alias: alias() }, t('contacts.aliasUpdated'))
                      }}
                    >
                      <label class="field">
                        <span>{t('contacts.localAlias')}</span>
                        <input value={alias()} maxLength={80} onInput={(event) => setAlias(event.currentTarget.value)} />
                      </label>
                      <button class="button button-secondary" type="submit" disabled={submitting()}>
                        {t('contacts.saveAlias')}
                      </button>
                    </form>

                    <div class="drawer-actions">
                      <Show when={actions().canAccept}>
                        <button class="button button-secondary" type="button" disabled={submitting()}
                          onClick={() => void mutateContact({ status: 'active' }, t('contacts.accepted'))}>{t('contacts.accept')}</button>
                      </Show>
                      <Show when={actions().canGrantChat}>
                        <button
                          class="button button-primary"
                          type="button"
                          disabled={submitting()}
                          title={t('contacts.grantChatTitle')}
                          onClick={grantChat}
                        >
                          {t('contacts.grantChat')}
                        </button>
                      </Show>
                      <Show when={actions().canRevokeChat}>
                        <button
                          class="button button-secondary"
                          type="button"
                          disabled={submitting()}
                          title={t('contacts.revokeChatTitle')}
                          onClick={revokeChat}
                        >
                          {t('contacts.revokeChat')}
                        </button>
                      </Show>
                      <Show when={actions().canBlock}>
                        <button
                          class="button button-danger-quiet"
                          type="button"
                          disabled={submitting()}
                          onClick={() => void mutateContact({ status: 'blocked' }, t('contacts.blocked'))}
                        >
                          {t('contacts.block')}
                        </button>
                      </Show>
                      <Show when={actions().canRestore}>
                        <button
                          class="button button-secondary"
                          type="button"
                          disabled={submitting()}
                          onClick={() => void mutateContact({ status: 'active' }, t('contacts.restored'))}
                        >
                          {t('contacts.restore')}
                        </button>
                      </Show>
                      <button
                        class="button button-danger-quiet"
                        type="button"
                        disabled={submitting()}
                        onClick={() => setConfirmDelete({ names: [contact.name] })}
                      >
                        {t('common.delete')}
                      </button>
                    </div>
                  </div>
                )
              }}
            </Show>
          </aside>
        </div>
      </Show>

      <Dialog
        open={confirmDelete() !== null}
        title={t('contacts.deleteTitle', { count: confirmDelete()?.names.length ?? 0 })}
        onClose={() => !submitting() && setConfirmDelete(null)}
      >
        <p class="dialog-copy">
          {t('contacts.deleteDetail')}
        </p>
        <div class="dialog-actions">
          <button class="button button-secondary" type="button" disabled={submitting()} onClick={() => setConfirmDelete(null)}>
            {t('common.cancel')}
          </button>
          <button class="button button-danger" type="button" disabled={submitting()} onClick={removeContacts}>
            {submitting() ? t('common.deleting') : t('common.delete')}
          </button>
        </div>
      </Dialog>
    </>
  )
}
