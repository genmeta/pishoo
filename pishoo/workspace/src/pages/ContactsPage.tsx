import { For, Show, createEffect, createMemo, createResource, createSignal, onCleanup, onMount } from 'solid-js'

import { api } from '../api/client'
import type { Contact, DirectoryEntry } from '../api/types'
import { chatApi } from '../chat/api'
import { Avatar, Dialog, EmptyState, ErrorState, LoadingState, Pagination, StatusBadge } from '../components/Ui'
import type { ContactFollowupReason } from '../i18n'
import { contactClassLabel, contactFollowupLabel, useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { chatCapabilityLabel, chatDirectionLabel, chatDirectionState } from '../lib/chatCapability'
import { contactDisplayName, contactStatus, displayIdentityName, errorMessage } from '../lib/format'
import { contactChatPath, contactPath } from '../lib/routes'

type ConfirmDelete = { names: string[] } | null
type ContactCategory = 'attention' | 'saved' | 'blocked'
const CONTACT_CATEGORIES: ContactCategory[] = ['attention', 'saved', 'blocked']
const CATEGORY_LABEL_KEYS = {
  attention: 'contacts.category.attention',
  saved: 'contacts.category.saved',
  blocked: 'contacts.category.blocked',
} as const

function ContactIdentity(props: { contact: Contact; large?: boolean; detail?: boolean }) {
  const loadProfile = abortable((name: string, signal: AbortSignal) => api.publicProfile(name, signal))
  const source = () => ['active', 'blocked'].includes(contactStatus(props.contact.status))
    ? props.contact.name
    : null
  const [profile] = createResource(source, loadProfile)
  const resolvedProfile = () => profile.error ? undefined : profile()
  const identityName = () => displayIdentityName(props.contact.name)
  const label = () => contactDisplayName(props.contact.name, props.contact.alias, resolvedProfile()?.display_name)
  return (
    <>
      <Avatar name={label()} src={resolvedProfile()?.avatar_url} large={props.large} />
      <span classList={{ 'contact-identity-detail': props.detail }}>
        <strong>{label()}</strong>
        <Show when={identityName() !== label()}>
          <small>{identityName()}</small>
        </Show>
      </span>
    </>
  )
}

export default function ContactsPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
  contactName?: string
  navigate: (path: string) => void
  returnTo?: string
  onResolved?: () => void
}) {
  const { date, t } = useI18n()
  const [page, setPage] = createSignal(1)
  const [category, setCategory] = createSignal<ContactCategory | null>(null)
  const [sort, setSort] = createSignal('updated_at')
  const [order, setOrder] = createSignal('desc')
  const [revision, setRevision] = createSignal(0)
  const selectedName = () => props.contactName ?? null
  const openContact = (name: string) => props.navigate(contactPath(name))
  const openChat = (name: string) => props.navigate(contactChatPath(name))
  const closeContact = () => props.navigate(props.returnTo ?? '/std/workspace/contacts')
  const [selected, setSelected] = createSignal<Set<string>>(new Set())
  const [confirmDelete, setConfirmDelete] = createSignal<ConfirmDelete>(null)
  const [alias, setAlias] = createSignal('')
  const [submitting, setSubmitting] = createSignal(false)
  const loadContacts = abortable(
    async ([currentSort, currentOrder]: readonly [string, string, number], signal: AbortSignal) => {
      const [items, directory] = await Promise.all([
        api.allContacts(currentSort, currentOrder, signal),
        api.contactDirectory(signal),
      ])
      return { items, directory }
    },
  )
  const loadContact = abortable((name: string, signal: AbortSignal) => api.contact(name, signal))
  const loadChatCapability = abortable((name: string, signal: AbortSignal) => chatApi.capability(name, signal))
  const [contacts] = createResource(
    () => [sort(), order(), revision()] as const,
    loadContacts,
  )
  const [detail, detailActions] = createResource(selectedName, loadContact)
  const [chatCapability, chatCapabilityActions] = createResource(selectedName, loadChatCapability)
  const directoryEntries = createMemo(() => new Map<string, DirectoryEntry>(
    contacts()?.directory.map((entry) => [entry.name, entry]) ?? [],
  ))
  const directory = createMemo(() => new Map<string, boolean>(
    contacts()?.directory.map((entry) => [entry.name, entry.saved]) ?? [],
  ))
  const chatAvailable = createMemo(() => new Set(
    contacts()?.directory.filter((entry) => entry.chat_available).map((entry) => entry.name) ?? [],
  ))
  const followupReason = (contact: Contact): ContactFollowupReason | null => {
    if (contactStatus(contact.status) !== 'active') return null
    const entry = directoryEntries().get(contact.name)
    if (!entry) return 'unlisted'
    if (!Object.prototype.hasOwnProperty.call(contact.requested_access, '/std/message')
      || entry.remote_chat_granted === true) return null
    return entry.remote_chat_granted === false ? 'remote-not-granted' : 'remote-unknown'
  }
  const directoryContacts = () => contacts()?.items.filter((contact) =>
    directory().has(contact.name) || contactStatus(contact.status) === 'active') ?? []
  const inCategory = (contact: Contact, current: ContactCategory | null) => {
    if (current === 'attention') return followupReason(contact) !== null
    if (current === 'saved') return directory().get(contact.name) === true
    if (current === 'blocked') return contactStatus(contact.status) === 'blocked'
    return contactStatus(contact.status) === 'active'
      && chatAvailable().has(contact.name)
      && directory().get(contact.name) !== true
      && followupReason(contact) === null
  }
  const categoryCount = (current: ContactCategory) => directoryContacts()
    .filter((contact) => inCategory(contact, current)).length
  const filtered = () => directoryContacts().filter((contact) => inCategory(contact, category()))
  const followupLabel = (contact: Contact) => {
    const reason = followupReason(contact)
    return reason ? contactFollowupLabel(t, reason) : null
  }
  const followupDetail = (contact: Contact) => {
    const reason = followupReason(contact)
    return reason ? contactFollowupLabel(t, reason, 'detail') : undefined
  }
  const visible = () => filtered().slice((page() - 1) * 20, page() * 20)

  const refresh = () => setRevision((value) => value + 1)
  onMount(() => {
    const timer = window.setInterval(() => {
      if (document.hidden || submitting()) return
      // Approval is reconciled asynchronously by both profiles. Keep the list
      // and open detail current until the reverse permission is confirmed.
      if (!contacts.loading && !contacts.error
        && directoryContacts().some((contact) => followupReason(contact) !== null)) refresh()
      if (selectedName() && !chatCapability.loading && !chatCapability.error
        && chatCapability()?.contact_status === 'active' && chatCapability()?.remote_grant !== true) {
        void chatCapabilityActions.refetch()
      }
    }, 3000)
    onCleanup(() => window.clearInterval(timer))
  })
  createEffect(() => {
    if (!contacts()) return
    const lastPage = Math.max(1, Math.ceil(filtered().length / 20))
    if (page() > lastPage) setPage(lastPage)
  })
  const changeCategory = (next: ContactCategory) => {
    setCategory((current) => current === next ? null : next)
    setPage(1)
    setSelected(new Set<string>())
  }
  createEffect(() => {
    setAlias(detail()?.alias ?? '')
  })
  const toggle = (name: string) => {
    const next = new Set(selected())
    if (next.has(name)) next.delete(name)
    else next.add(name)
    setSelected(next)
  }

  const toggleAll = () => {
    const names = visible().map((contact) => contact.name)
    const allSelected = names.length > 0 && names.every((name) => selected().has(name))
    const next = new Set(selected())
    for (const name of names) {
      if (allSelected) next.delete(name)
      else next.add(name)
    }
    setSelected(next)
  }

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
      await chatCapabilityActions.refetch()
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
    void chatApi.grantCapability(name, 'chat').then(async () => {
      props.notify(t('contacts.chatGranted'))
      refresh()
      props.onResolved?.()
      await detailActions.refetch()
      await chatCapabilityActions.refetch()
    }).catch((error) => {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    }).finally(() => setSubmitting(false))
  }

  const revokeChat = () => {
    const name = selectedName()
    if (!name || submitting()) return
    setSubmitting(true)
    void chatApi.revokeCapability(name, 'chat').then(async () => {
      props.notify(t('contacts.chatRevoked'))
      refresh()
      props.onResolved?.()
      await detailActions.refetch()
      await chatCapabilityActions.refetch()
    }).catch((error) => {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    }).finally(() => setSubmitting(false))
  }

  const setIdentitySaved = async (name: string, saved: boolean) => {
    if (submitting()) return
    setSubmitting(true)
    try {
      if (saved) await api.unsaveContactIdentity(name)
      else await api.saveContactIdentity(name)
      props.notify(t(saved ? 'contacts.identityUnsaved' : 'contacts.identitySaved'))
      refresh()
      props.onResolved?.()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setSubmitting(false)
    }
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
      setSelected((current) => {
        const next = new Set(current)
        names.forEach((name) => next.delete(name))
        return next
      })
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
      canGrantChat: ['pending', 'transfered', 'active'].includes(status)
        && Object.prototype.hasOwnProperty.call(contact.requested_access, '/std/message')
        && chatCapability()?.can_receive !== true,
      canRevokeChat: status === 'active' && chatCapability()?.can_receive === true,
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
            onClick={() => props.navigate('/std/workspace/contacts/new')}>
            {t('nav.contactNew')}
          </button>
          <button class="button button-secondary" type="button" onClick={refresh} disabled={contacts.loading}>
            {t('common.refresh')}
          </button>
        </div>
      </header>

      <div class="contact-category-filters" role="group" aria-label={t('contacts.category.label')}>
        <For each={CONTACT_CATEGORIES}>{(item) =>
          <button class="button button-secondary" type="button"
            classList={{ active: category() === item }} aria-pressed={category() === item}
            aria-label={`${t(CATEGORY_LABEL_KEYS[item])} ${categoryCount(item)}`}
            onClick={() => changeCategory(item)}>
            {t(CATEGORY_LABEL_KEYS[item])}
            <span class="contact-category-count">{categoryCount(item)}</span>
          </button>
        }</For>
      </div>

      <div class="toolbar toolbar-wrap">
        <Show
          when={selected().size}
          fallback={<span class="toolbar-meta">{t('contacts.count', { count: filtered().length })}</span>}
        >
          <div class="bulk-actions">
            <strong>{t('contacts.selected', { count: selected().size })}</strong>
            <button
              class="button button-danger-quiet button-small"
              type="button"
              onClick={() => setConfirmDelete({ names: [...selected()] })}
            >
              {t('common.delete')}
            </button>
          </div>
        </Show>
        <div class="filter-group">
          <label>
            <span class="sr-only">{t('contacts.sortBy')}</span>
            <select
              value={sort()}
              onChange={(event) => {
                setSort(event.currentTarget.value)
                setPage(1)
              }}
            >
              <option value="updated_at">{t('contacts.updated')}</option>
              <option value="created_at">{t('contacts.created')}</option>
              <option value="name">{t('contacts.name')}</option>
              <option value="alias">{t('contacts.alias')}</option>
              <option value="class">{t('contacts.class')}</option>
            </select>
          </label>
          <label>
            <span class="sr-only">{t('contacts.sortDirection')}</span>
            <select
              value={order()}
              onChange={(event) => {
                setOrder(event.currentTarget.value)
                setPage(1)
              }}
            >
              <option value="desc">{t('contacts.descending')}</option>
              <option value="asc">{t('contacts.ascending')}</option>
            </select>
          </label>
        </div>
      </div>

      <section class="data-panel" aria-label={t('contacts.section')}>
        <Show when={contacts.error}>
          <ErrorState message={errorMessage(contacts.error, t('common.unexpectedError'))} onRetry={refresh} />
        </Show>
        <Show when={contacts.loading && !contacts()}>
          <LoadingState label={t('contacts.loading')} />
        </Show>
        <Show when={!contacts.error && contacts() && filtered().length === 0}>
          <EmptyState title={t(category() === null ? 'contacts.empty' : 'contacts.emptyCategory')} />
        </Show>
        <Show when={!contacts.error && filtered().length}>
          <div class="table-scroll">
            <table>
              <thead>
                <tr>
                  <th class="checkbox-cell">
                    <input
                      type="checkbox"
                      aria-label={t('contacts.selectAll')}
                      checked={visible().length > 0 && visible().every((contact) => selected().has(contact.name))}
                      onChange={toggleAll}
                    />
                  </th>
                  <th>{t('contacts.contact')}</th>
                  <th>{t('contacts.status')}</th>
                  <th>{t('contacts.class')}</th>
                  <th><span class="sr-only">{t('common.actions')}</span></th>
                </tr>
              </thead>
              <tbody>
                <For each={visible()}>
                  {(contact) => (
                    <tr classList={{ selected: selectedName() === contact.name }}>
                      <td class="checkbox-cell">
                        <input
                          type="checkbox"
                          aria-label={t('contacts.select', { name: displayIdentityName(contact.name) })}
                          checked={selected().has(contact.name)}
                          onChange={() => toggle(contact.name)}
                        />
                      </td>
                      <td>
                        <button class="contact-link" type="button" onClick={() => openContact(contact.name)}>
                          <ContactIdentity contact={contact} />
                        </button>
                      </td>
                      <td>
                        <div class="contact-row-status">
                          <Show when={contactStatus(contact.status) !== 'active'
                            || inCategory(contact, null)}>
                            <StatusBadge value={contactStatus(contact.status)} />
                          </Show>
                          <Show when={followupLabel(contact)}>
                            <span class="contact-status-note" title={followupDetail(contact)}>
                              {followupLabel(contact)}
                            </span>
                          </Show>
                          <Show when={directory().get(contact.name)}>
                            <span class="contact-saved-note">{t('contacts.savedMarker')}</span>
                          </Show>
                        </div>
                      </td>
                      <td>{contactClassLabel(t, contact.class)}</td>
                      <td>
                        <div class="row-actions contact-row-actions">
                          <Show when={followupReason(contact) === 'unlisted'}>
                            <button class="button button-secondary button-small" type="button" disabled={submitting()}
                              onClick={() => void setIdentitySaved(contact.name, false)}>
                              {t('contacts.saveIdentity')}
                            </button>
                          </Show>
                          <Show when={chatAvailable().has(contact.name)}>
                            <button class="button button-secondary button-small" type="button"
                              aria-label={t('contacts.chatWith', { name: displayIdentityName(contact.name) })}
                              onClick={() => openChat(contact.name)}>
                              {t('contacts.openChat')}
                            </button>
                          </Show>
                          <button
                            class="icon-button"
                            type="button"
                            aria-label={t('contacts.openContact', { name: displayIdentityName(contact.name) })}
                            title={t('contacts.openDetails')}
                            onClick={() => openContact(contact.name)}
                          >
                            ›
                          </button>
                        </div>
                      </td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </div>
          <Pagination
            page={page()}
            pageSize={20}
            total={filtered().length}
            onPage={(next) => { setPage(next); setSelected(new Set<string>()) }}
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
                const incomingChat = () => chatDirectionState('incoming', contact, chatCapability())
                const outgoingChat = () => chatDirectionState('outgoing', contact, chatCapability())
                return (
                  <div class="drawer-content">
                    <div class="contact-heading">
                      <ContactIdentity contact={contact} large detail />
                      <StatusBadge value={actions().status} />
                      <Show when={directory().get(contact.name)}>
                        <span class="method">{t('contacts.savedIdentity')}</span>
                      </Show>
                    </div>

                    <dl class="metadata-grid">
                      <Show when={contact.description}>
                        <div class="wide"><dt>{t('contacts.description')}</dt><dd>{contact.description}</dd></div>
                      </Show>
                      <div><dt>{t('contacts.alias')}</dt><dd>{contact.alias || t('common.none')}</dd></div>
                      <div><dt>{t('contacts.class')}</dt><dd>{contactClassLabel(t, contact.class)}</dd></div>
                      <div><dt>{t('contacts.updated')}</dt><dd>{date(contact.updated_at)}</dd></div>
                      <div><dt>{t('contacts.created')}</dt><dd>{date(contact.created_at)}</dd></div>
                      <div><dt>{t('contacts.expires')}</dt><dd>{date(contact.expired_after)}</dd></div>
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

                    <section class="detail-section capability-status-section">
                      <h3>{t('contacts.capabilities')}</h3>
                      <div class="contact-capability-card">
                        <strong>{t('contacts.chatCapability')}</strong>
                        <span class="status">{chatCapabilityLabel(contact, t, chatCapability())}</span>
                      </div>
                      <div class="capability-direction-grid">
                        <div class="capability-direction-row">
                          <strong>{t('contacts.chatDirectionIncoming')}</strong>
                          <span class="status">{chatDirectionLabel(incomingChat(), t)}</span>
                        </div>
                        <div class="capability-direction-row">
                          <strong>{t('contacts.chatDirectionOutgoing')}</strong>
                          <span class="status">{chatDirectionLabel(outgoingChat(), t)}</span>
                        </div>
                      </div>
                    </section>

                    <div class="drawer-actions">
                      <button
                        class="button button-secondary"
                        type="button"
                        disabled={submitting()}
                        onClick={() => void setIdentitySaved(contact.name, directory().get(contact.name) === true)}
                      >
                        {t(directory().get(contact.name) ? 'contacts.unsaveIdentity' : 'contacts.saveIdentity')}
                      </button>
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
