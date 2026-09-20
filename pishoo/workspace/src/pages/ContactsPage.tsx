import { For, Show, createEffect, createResource, createSignal } from 'solid-js'

import { api } from '../api/client'
import type { Contact, GrantedAccess, RequestedAccess } from '../api/types'
import { Dialog, EmptyState, ErrorState, LoadingState, Pagination, StatusBadge } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { contactStatus, errorMessage, initials } from '../lib/format'

type ConfirmDelete = { names: string[] } | null

function RequestedAccessList(props: { value: RequestedAccess }) {
  const { t } = useI18n()
  const entries = () => Object.entries(props.value)
  return (
    <Show when={entries().length} fallback={<span class="muted">{t('common.none')}</span>}>
      <div class="access-list">
        <For each={entries()}>
          {([path, methods]) => (
            <div class="access-row">
              <code>{path}</code>
              <div class="tag-list">
                <For each={methods}>{(method) => <span class="method">{method}</span>}</For>
              </div>
            </div>
          )}
        </For>
      </div>
    </Show>
  )
}

function GrantedAccessList(props: { value: GrantedAccess }) {
  const { t } = useI18n()
  const entries = () => Object.entries(props.value)
  return (
    <Show when={entries().length} fallback={<span class="muted">{t('common.none')}</span>}>
      <div class="access-list">
        <For each={entries()}>
          {([path, effects]) => (
            <div class="access-row access-row-stacked">
              <code>{path}</code>
              <For each={(['allow', 'review', 'deny'] as const).filter((effect) => effects[effect].length)}>
                {(effect) => (
                  <div class="effect-line">
                    <StatusBadge value={effect} />
                    <div class="tag-list">
                      <For each={effects[effect]}>{(method) => <span class="method">{method}</span>}</For>
                    </div>
                  </div>
                )}
              </For>
            </div>
          )}
        </For>
      </div>
    </Show>
  )
}

export default function ContactsPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
}) {
  const { date, t } = useI18n()
  const [page, setPage] = createSignal(1)
  const [sort, setSort] = createSignal('updated_at')
  const [order, setOrder] = createSignal('desc')
  const [revision, setRevision] = createSignal(0)
  const [selectedName, setSelectedName] = createSignal<string | null>(null)
  const [selected, setSelected] = createSignal<Set<string>>(new Set())
  const [confirmDelete, setConfirmDelete] = createSignal<ConfirmDelete>(null)
  const [alias, setAlias] = createSignal('')
  const [submitting, setSubmitting] = createSignal(false)
  const loadContacts = abortable(
    ([currentPage, currentSort, currentOrder]: readonly [number, string, string, number], signal: AbortSignal) =>
      api.contacts(currentPage, currentSort, currentOrder, signal),
  )
  const loadContact = abortable((name: string, signal: AbortSignal) => api.contact(name, signal))
  const [contacts] = createResource(
    () => [page(), sort(), order(), revision()] as const,
    loadContacts,
  )
  const [detail, detailActions] = createResource(selectedName, loadContact)

  const refresh = () => setRevision((value) => value + 1)
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
    const names = contacts()?.items.map((contact) => contact.name) ?? []
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
      await detailActions.refetch()
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
      if (selectedName() && names.includes(selectedName()!)) setSelectedName(null)
      setConfirmDelete(null)
      refresh()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setSubmitting(false)
    }
  }

  const statusActions = (contact: Contact) => {
    const status = contactStatus(contact.status)
    return {
      canApprove: ['pending', 'transfered'].includes(status),
      canBlock: ['active', 'transfered'].includes(status),
      canRestore: status === 'blocked',
      status,
    }
  }

  return (
    <>
      <header class="page-header">
        <div>
          <p class="eyebrow">{t('contacts.eyebrow')}</p>
          <h1>{t('contacts.title')}</h1>
        </div>
        <button class="button button-secondary" type="button" onClick={refresh} disabled={contacts.loading}>
          {t('common.refresh')}
        </button>
      </header>

      <div class="toolbar toolbar-wrap">
        <Show
          when={selected().size}
          fallback={<span class="toolbar-meta">{t('contacts.count', { count: contacts()?.total ?? 0 })}</span>}
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
        <Show when={!contacts.error && contacts()?.items.length === 0}>
          <EmptyState title={t('contacts.empty')} detail={t('contacts.emptyDetail')} />
        </Show>
        <Show when={contacts()?.items.length}>
          <div class="table-scroll">
            <table>
              <thead>
                <tr>
                  <th class="checkbox-cell">
                    <input
                      type="checkbox"
                      aria-label={t('contacts.selectAll')}
                      checked={contacts()?.items.every((contact) => selected().has(contact.name))}
                      onChange={toggleAll}
                    />
                  </th>
                  <th>{t('contacts.contact')}</th>
                  <th>{t('contacts.status')}</th>
                  <th>{t('contacts.class')}</th>
                  <th>{t('contacts.updated')}</th>
                  <th>{t('contacts.expires')}</th>
                  <th><span class="sr-only">{t('contacts.open')}</span></th>
                </tr>
              </thead>
              <tbody>
                <For each={contacts()?.items}>
                  {(contact) => (
                    <tr classList={{ selected: selectedName() === contact.name }}>
                      <td class="checkbox-cell">
                        <input
                          type="checkbox"
                          aria-label={t('contacts.select', { name: contact.name })}
                          checked={selected().has(contact.name)}
                          onChange={() => toggle(contact.name)}
                        />
                      </td>
                      <td>
                        <button class="contact-link" type="button" onClick={() => setSelectedName(contact.name)}>
                          <span class="avatar" aria-hidden="true">{initials(contact.alias || contact.name)}</span>
                          <span>
                            <strong>{contact.alias || contact.name}</strong>
                            <small>{contact.alias ? contact.name : contact.description || t('contacts.noDescription')}</small>
                          </span>
                        </button>
                      </td>
                      <td><StatusBadge value={contactStatus(contact.status)} /></td>
                      <td>{contact.class || t('contacts.unclassified')}</td>
                      <td>{date(contact.updated_at)}</td>
                      <td>{date(contact.expired_after)}</td>
                      <td>
                        <button
                          class="icon-button"
                          type="button"
                          aria-label={t('contacts.openContact', { name: contact.name })}
                          title={t('contacts.openDetails')}
                          onClick={() => setSelectedName(contact.name)}
                        >
                          ›
                        </button>
                      </td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </div>
          <Pagination
            page={contacts()?.page ?? page()}
            pageSize={contacts()?.page_size ?? 20}
            total={contacts()?.total ?? 0}
            onPage={setPage}
          />
        </Show>
      </section>

      <Show when={selectedName()}>
        <div class="drawer-backdrop" onMouseDown={(event) => event.target === event.currentTarget && setSelectedName(null)}>
          <aside class="detail-drawer" aria-label={t('contacts.details')}>
            <header class="drawer-header">
              <div>
                <p class="eyebrow">{t('contacts.details')}</p>
                <h2>{detail()?.alias || detail()?.name || selectedName()}</h2>
              </div>
              <button class="icon-button" type="button" aria-label={t('contacts.closeDetails')} onClick={() => setSelectedName(null)}>
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
                      <span class="avatar avatar-large" aria-hidden="true">{initials(contact.alias || contact.name)}</span>
                      <div>
                        <strong>{contact.name}</strong>
                        <span>{contact.description || t('contacts.noDescription')}</span>
                      </div>
                      <StatusBadge value={actions().status} />
                    </div>

                    <dl class="metadata-grid">
                      <div><dt>{t('contacts.class')}</dt><dd>{contact.class || t('contacts.unclassified')}</dd></div>
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

                    <section class="detail-section">
                      <h3>{t('contacts.requestedAccess')}</h3>
                      <RequestedAccessList value={contact.requested_access} />
                    </section>
                    <section class="detail-section">
                      <h3>{t('contacts.grantedAccess')}</h3>
                      <GrantedAccessList value={contact.granted_access} />
                    </section>
                    <section class="detail-section">
                      <h3>{t('contacts.declaredAccess')}</h3>
                      <GrantedAccessList value={contact.offers} />
                    </section>

                    <div class="drawer-actions">
                      <Show when={actions().canApprove}>
                        <button
                          class="button button-primary"
                          type="button"
                          disabled={submitting()}
                          title={t('contacts.approveTitle')}
                          onClick={() => void mutateContact({ status: 'active' }, t('contacts.approved'))}
                        >
                          {t('contacts.approve')}
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
