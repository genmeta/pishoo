import { For, Show, createResource, createSignal } from 'solid-js'

import { api } from '../api/client'
import type { OutboundContactRequest } from '../api/types'
import { Dialog, EmptyState, ErrorState, LoadingState, Pagination, StatusBadge } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { displayIdentityName, errorMessage } from '../lib/format'

export default function OutboundRequestsPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
}) {
  const { date, t } = useI18n()
  const [page, setPage] = createSignal(1)
  const [revision, setRevision] = createSignal(0)
  const [working, setWorking] = createSignal<number | null>(null)
  const [deleting, setDeleting] = createSignal<OutboundContactRequest | null>(null)
  const load = abortable(([currentPage]: readonly [number, number], signal: AbortSignal) =>
    api.outboundRequests(currentPage, signal))
  const [requests] = createResource(() => [page(), revision()] as const, load)
  const reload = () => setRevision((value) => value + 1)

  const check = async (id: number) => {
    if (working() !== null) return
    setWorking(id)
    try {
      await api.refreshOutboundRequest(id)
      reload()
      props.notify(t('outbound.checked'))
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setWorking(null)
    }
  }

  const remove = async () => {
    const current = deleting()
    if (!current || working() !== null) return
    setWorking(current.id)
    try {
      await api.deleteOutboundRequest(current.id)
      setDeleting(null)
      props.notify(t('outbound.deleted'))
      if (requests()?.items.length === 1 && page() > 1) setPage(page() - 1)
      else reload()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setWorking(null)
    }
  }

  return (
    <>
      <header class="page-header">
        <h1>{t('nav.contactRequests')}</h1>
        <button class="button button-secondary" type="button" disabled={requests.loading} onClick={reload}>
          {t('common.refresh')}
        </button>
      </header>
      <Show when={requests.error}><ErrorState message={errorMessage(requests.error, t('common.unexpectedError'))} onRetry={reload} /></Show>
      <Show when={requests.loading && !requests()}><LoadingState label={t('outbound.loading')} /></Show>
      <Show when={!requests.error && requests()?.items.length === 0}>
        <EmptyState title={t('outbound.empty')} />
      </Show>
      <Show when={!requests.error && requests()?.items.length}>
        <div class="toolbar"><span class="toolbar-meta">{t('outbound.count', { count: requests()?.total ?? 0 })}</span></div>
        <section class="data-panel" aria-label={t('nav.contactRequests')}>
          <div class="table-scroll"><table>
            <thead><tr><th>{t('outbound.target')}</th><th>{t('contacts.status')}</th><th>{t('contacts.expires')}</th>
              <th>{t('outbound.lastChecked')}</th><th>{t('common.actions')}</th></tr></thead>
            <tbody><For each={requests()?.items}>{(item) => (
              <tr><td><strong>{displayIdentityName(item.target_name)}</strong><Show when={item.description}><small class="muted outbound-description">{item.description}</small></Show></td>
                <td><StatusBadge value={item.status} /><Show when={item.error_message}><small class="field-error outbound-description">{item.error_message}</small></Show></td>
                <td><Show when={item.status === 'queued' || item.status === 'pending' || item.status === 'expired'} fallback={t('common.none')}>
                  <small>{item.status === 'queued' ? t('outbound.deliveryDeadline') : t('contacts.expires')}</small><br />{date(item.expired_after)}
                </Show></td><td>{item.last_checked_at ? date(item.last_checked_at) : t('common.none')}</td>
                <td><div class="row-actions">
                  <button class="button button-secondary button-small" type="button" disabled={working() !== null || !['queued', 'pending', 'active'].includes(item.status)}
                    onClick={() => void check(item.id)}>{working() === item.id ? t('outbound.checking') : t('outbound.check')}</button>
                  <button class="button button-danger-quiet button-small" type="button" disabled={working() !== null}
                    onClick={() => setDeleting(item)}>{t('common.delete')}</button>
                </div></td>
              </tr>
            )}</For></tbody>
          </table></div>
          <Pagination page={requests()?.page ?? page()} pageSize={requests()?.page_size ?? 20}
            total={requests()?.total ?? 0} onPage={setPage} />
        </section>
      </Show>
      <Dialog open={deleting() !== null} title={t('outbound.deleteTitle')} onClose={() => working() === null && setDeleting(null)}>
        <p>{t(deleting()?.status === 'queued' ? 'outbound.cancelQueuedHint' : 'outbound.deleteHint',
          { name: displayIdentityName(deleting()?.target_name ?? '') })}</p>
        <div class="dialog-actions">
          <button class="button button-secondary" type="button" disabled={working() !== null} onClick={() => setDeleting(null)}>{t('common.cancel')}</button>
          <button class="button button-danger" type="button" disabled={working() !== null} onClick={() => void remove()}>{t('common.delete')}</button>
        </div>
      </Dialog>
    </>
  )
}
