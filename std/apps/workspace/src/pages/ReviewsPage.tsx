import { For, Show, createMemo, createResource, createSignal, onCleanup, onMount } from 'solid-js'

import { ApiError, api } from '../api/client'
import type { AccessApproval, Approval, ContactApproval } from '../api/types'
import ContactDetailsDrawer from '../components/ContactDetailsDrawer'
import { Dialog, EmptyState, ErrorState, LoadingState, Pagination } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { displayIdentityName, errorMessage } from '../lib/format'

interface Decision {
  review: AccessApproval
  action: 'allow' | 'deny'
}

type ApprovalTab = 'pending' | 'expired'

function approvalKey(item: Approval): string {
  return item.kind === 'contact' ? `contact:${item.request_id}:${item.subject_id}` : `access:${item.review_kind}:${item.id}`
}

export default function ReviewsPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
  onRefresh?: () => void
}) {
  const { date, t } = useI18n()
  const [page, setPage] = createSignal(1)
  const [revision, setRevision] = createSignal(0)
  const [tab, setTab] = createSignal<ApprovalTab>('pending')
  const [decision, setDecision] = createSignal<Decision | null>(null)
  const [deleteCandidate, setDeleteCandidate] = createSignal<Approval | null>(null)
  const [deleting, setDeleting] = createSignal(false)
  const [submitting, setSubmitting] = createSignal(false)
  const [contactWorking, setContactWorking] = createSignal<number | null>(null)
  const [selectedContactName, setSelectedContactName] = createSignal<string | null>(null)
  const fetchApprovals = async ([status, currentPage]: readonly [ApprovalTab, number, number], signal: AbortSignal) => {
    const result = await api.approvals(status, currentPage, signal)
    signal.throwIfAborted()
    return result
  }
  const load = abortable(fetchApprovals)
  const loadBackground = abortable(fetchApprovals)
  const source = createMemo(() => [tab(), page(), revision()] as const)
  const [catalog] = createResource(() => revision(), () => api.capabilities())
  const [approvals, approvalActions] = createResource(source, load)
  let backgroundLoading = false

  const refresh = () => {
    setRevision((value) => value + 1)
    props.onRefresh?.()
  }
  const changeTab = (value: ApprovalTab) => {
    setPage(1)
    setTab(value)
  }
  const refreshInBackground = async () => {
    if (document.hidden || submitting() || deleting() || contactWorking() !== null
      || approvals.loading || approvals.error || backgroundLoading) return
    const currentSource = source()
    backgroundLoading = true
    try {
      const result = await loadBackground(currentSource)
      if (source() !== currentSource) return
      approvalActions.mutate(previous => {
        const existing = new Map<string, Approval>(previous?.items.map(item => [approvalKey(item), item]))
        return {
          ...result,
          items: result.items.map(item => {
            const current = existing.get(approvalKey(item))
            if (current && Object.keys(current).length === Object.keys(item).length
              && Object.entries(item).every(([key, value]) => Reflect.get(current, key) === value)) {
              return current
            }
            return item
          }),
        }
      })
      props.onRefresh?.()
    } catch {
      // Keep the last successful snapshot; the next poll retries.
    } finally {
      backgroundLoading = false
    }
  }
  const onVisibilityChange = () => {
    void refreshInBackground()
  }
  onMount(() => {
    document.addEventListener('visibilitychange', onVisibilityChange)
    const timer = window.setInterval(() => void refreshInBackground(), 3000)
    onCleanup(() => window.clearInterval(timer))
  })
  onCleanup(() => document.removeEventListener('visibilitychange', onVisibilityChange))

  const decideContact = async (request: ContactApproval, action: 'accept' | 'deny') => {
    if (contactWorking() !== null) return
    setContactWorking(request.request_id)
    try {
      const current = await api.contact(request.contact_name)
      if (Reflect.get(current, 'id') !== request.request_id || current.subject_id !== request.subject_id) {
        throw new ApiError(409, 'Contact request changed')
      }
      if (action === 'accept') {
        await api.patchContact(request.contact_name, { status: 'active' })
        props.notify(t('contacts.accepted'))
      } else {
        await api.deleteContact(request.contact_name)
        props.notify(t('reviews.contactDeclined'))
      }
      setPage(1)
      refresh()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setContactWorking(null)
    }
  }

  const decisionAction = () =>
    decision()?.action === 'allow' ? t('reviews.allow') : t('reviews.deny')
  const submitLabel = () => {
    if (submitting()) return t('reviews.submitting')
    return decision()?.action === 'allow' ? t('reviews.allow') : t('reviews.deny')
  }
  const decide = async () => {
    const current = decision()
    if (!current) return
    setSubmitting(true)
    try {
      await api.decideReview(current.review.id, current.action, current.review.review_kind)
      props.notify(current.action === 'allow' ? t('reviews.allowed') : t('reviews.denied'))
      setDecision(null)
      setPage(1)
      refresh()
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        props.notify(t('reviews.alreadyResolved'), 'error')
        setDecision(null)
        refresh()
      } else {
        props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
      }
    } finally {
      setSubmitting(false)
    }
  }
  const removeExpired = async () => {
    const current = deleteCandidate()
    if (!current || deleting()) return
    setDeleting(true)
    try {
      await api.deleteExpiredApproval(current)
      setDeleteCandidate(null)
      props.notify(t('reviews.deleted'))
      if (page() > 1 && approvals()?.items.length === 1) setPage(page() - 1)
      else refresh()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setDeleting(false)
    }
  }
  const openDecision = (review: AccessApproval, action: Decision['action']) => {
    setDecision({ review, action })
  }
  const applicant = (item: Approval) =>
    displayIdentityName(item.kind === 'contact' ? item.contact_name : item.visitor)
  const capabilityTitle = (item: Approval) => {
    if (item.kind === 'contact') return t('reviews.contactRequest')
    return catalog()?.find(capability => capability.endpoints.some(endpoint => endpoint.method === item.method && endpoint.path === item.api))?.title
      ?? `${item.method} ${item.api}`
  }

  return (
    <>
      <header class="page-header">
        <h1>{t('reviews.title')}</h1>
        <button class="button button-secondary" type="button" onClick={refresh} disabled={approvals.loading}>
          {t('common.refresh')}
        </button>
      </header>

      <div class="section-nav" role="group" aria-label={t('reviews.tabs')}>
        <button type="button" aria-pressed={tab() === 'pending'}
          classList={{ active: tab() === 'pending' }} onClick={() => changeTab('pending')}>
          {t('reviews.pendingTab')}
        </button>
        <button type="button" aria-pressed={tab() === 'expired'}
          classList={{ active: tab() === 'expired' }} onClick={() => changeTab('expired')}>
          {t('reviews.expiredTab')}
        </button>
      </div>

      <div class="toolbar">
        <span class="toolbar-meta" aria-live="polite">
          {approvals.loading ? t('reviews.updating')
            : t(tab() === 'pending' ? 'reviews.pendingCount' : 'reviews.expiredCount', { count: approvals()?.total ?? 0 })}
        </span>
      </div>

      <section class="data-panel" aria-label={t(tab() === 'pending' ? 'reviews.pendingSection' : 'reviews.expiredSection')}>
        <Show when={approvals.error}>
          <ErrorState message={errorMessage(approvals.error, t('common.unexpectedError'))} onRetry={refresh} />
        </Show>
        <Show when={approvals.loading}>
          <LoadingState label={t('reviews.loading')} />
        </Show>
        <Show when={!approvals.loading && !approvals.error && approvals()?.items.length === 0}>
          <EmptyState title={t(tab() === 'pending' ? 'reviews.clear' : 'reviews.expiredClear')} />
        </Show>
        <Show when={!approvals.loading && !approvals.error && approvals()?.items.length}>
          <div class="table-scroll">
            <table class="approval-table">
              <thead>
                <tr>
                  <th>{t('reviews.type')}</th>
                  <th>{t('reviews.requester')}</th>
                  <th>{t('capabilities.title')}</th>
                  <th>{t('reviews.requestedAt')}</th>
                  <th>{t('reviews.expiry')}</th>
                  <th><span class="sr-only">{t('common.actions')}</span></th>
                </tr>
              </thead>
              <tbody>
                <For each={approvals()?.items}>
                  {(item) => (
                    <tr>
                      <td class="approval-type-cell" data-label={t('reviews.type')}>
                        <span class="method">{t(item.kind === 'contact' ? 'reviews.contactRequest' : 'reviews.accessTab')}</span>
                      </td>
                      <td class="approval-requester-cell" data-label={t('reviews.requester')}>
                        {item.kind === 'contact'
                          ? <button class="contact-link" type="button" title={t('contacts.openDetails')}
                              onClick={() => setSelectedContactName(item.contact_name)}>
                              <strong>{applicant(item)}</strong>
                            </button>
                          : <strong>{applicant(item)}</strong>}
                      </td>
                      <td class="approval-request-cell" data-label={t('capabilities.title')}>
                        <strong>{capabilityTitle(item)}</strong>
                      </td>
                      <td class="approval-time-cell" data-label={t('reviews.requestedAt')}>{date(item.requested_at)}</td>
                      <td class="approval-expiry-cell" data-label={t('reviews.expiry')}>{item.kind === 'access' && item.review_kind === 'live' ? t('reviews.live') : date(item.expired_after)}</td>
                      <td class="approval-actions-cell" data-label={t('common.actions')}
                        aria-busy={tab() === 'pending' && item.kind === 'contact' && contactWorking() === item.request_id || tab() === 'expired' && deleting()}>
                        <Show when={tab() === 'pending'} fallback={
                          <div class="row-actions">
                            <button class="button button-danger-quiet button-small" type="button"
                              disabled={deleting()} onClick={() => setDeleteCandidate(item)}>
                              {t('common.delete')}
                            </button>
                          </div>
                        }>
                          {item.kind === 'contact'
                            ? <div class="row-actions">
                                <button class="button button-success button-small" type="button"
                                  disabled={contactWorking() !== null}
                                  onClick={() => void decideContact(item, 'accept')}>
                                  {t('contacts.accept')}
                                </button>
                                <button class="button button-danger-quiet button-small" type="button"
                                  disabled={contactWorking() !== null}
                                  onClick={() => void decideContact(item, 'deny')}>
                                  {t('reviews.deny')}
                                </button>
                              </div>
                            : <div class="row-actions">
                                <button class="button button-success button-small" type="button"
                                  onClick={() => openDecision(item, 'allow')}>{t('reviews.allow')}</button>
                                <button class="button button-danger-quiet button-small" type="button"
                                  onClick={() => openDecision(item, 'deny')}>{t('reviews.deny')}</button>
                              </div>}
                        </Show>
                      </td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </div>
          <Pagination
            page={approvals()?.page ?? page()}
            pageSize={approvals()?.page_size ?? 20}
            total={approvals()?.total ?? 0}
            onPage={setPage}
          />
        </Show>
      </section>

      <Show when={selectedContactName()} keyed>
        {(contactName) => (
          <ContactDetailsDrawer
            contactName={contactName}
            onClose={() => setSelectedContactName(null)}
          />
        )}
      </Show>

      <Dialog
        open={decision() !== null}
        title={t('reviews.decisionTitle', {
          action: decisionAction(),
          id: decision()?.review.id ?? '',
        })}
        onClose={() => !submitting() && setDecision(null)}
      >
        <div class="decision-summary">
          <strong>{decision() ? capabilityTitle(decision()!.review) : ''}</strong>
          <span>{displayIdentityName(decision()?.review.visitor ?? '')}</span>
        </div>
        <p>{t('reviews.decisionPersists')}</p>
        <div class="dialog-actions">
          <button class="button button-secondary" type="button" onClick={() => setDecision(null)} disabled={submitting()}>
            {t('common.cancel')}
          </button>
          <button
            class={`button ${decision()?.action === 'allow' ? 'button-success' : 'button-danger'}`}
            type="button"
            onClick={decide}
            disabled={submitting()}
          >
            {submitLabel()}
          </button>
        </div>
      </Dialog>

      <Dialog
        open={deleteCandidate() !== null}
        title={t('reviews.deleteExpiredTitle')}
        onClose={() => !deleting() && setDeleteCandidate(null)}
      >
        <p>{t('reviews.deleteExpiredHint', {
          name: deleteCandidate() ? applicant(deleteCandidate()!) : '',
        })}</p>
        <div class="dialog-actions">
          <button class="button button-secondary" type="button" disabled={deleting()}
            onClick={() => setDeleteCandidate(null)}>{t('common.cancel')}</button>
          <button class="button button-danger" type="button" disabled={deleting()}
            onClick={() => void removeExpired()}>{deleting() ? t('common.deleting') : t('common.delete')}</button>
        </div>
      </Dialog>
    </>
  )
}
