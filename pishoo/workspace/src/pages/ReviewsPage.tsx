import { For, Show, createResource, createSignal, onCleanup, onMount } from 'solid-js'

import { ApiError, api } from '../api/client'
import type { AccessApproval, Approval, CapabilityApproval } from '../api/types'
import { chatApi } from '../chat/api'
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

function defaultExpiry(): string {
  const date = new Date(Date.now() + 60 * 60 * 1000)
  const local = new Date(date.getTime() - date.getTimezoneOffset() * 60_000)
  return local.toISOString().slice(0, 16)
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
  const [expiry, setExpiry] = createSignal(defaultExpiry())
  const [submitting, setSubmitting] = createSignal(false)
  const [capabilityWorking, setCapabilityWorking] = createSignal<number | null>(null)
  const [selectedContactName, setSelectedContactName] = createSignal<string | null>(null)
  const load = abortable(([status, currentPage]: readonly [ApprovalTab, number, number], signal: AbortSignal) =>
    api.approvals(status, currentPage, signal))
  const [approvals] = createResource(() => [tab(), page(), revision()] as const, load)

  const refresh = () => {
    setRevision((value) => value + 1)
    props.onRefresh?.()
  }
  const changeTab = (value: ApprovalTab) => {
    setPage(1)
    setTab(value)
  }
  const onVisibilityChange = () => {
    if (!document.hidden) refresh()
  }
  onMount(() => document.addEventListener('visibilitychange', onVisibilityChange))
  onCleanup(() => document.removeEventListener('visibilitychange', onVisibilityChange))

  const decideCapability = async (request: CapabilityApproval, action: 'grant' | 'deny') => {
    if (capabilityWorking() !== null) return
    setCapabilityWorking(request.request_id)
    try {
      if (action === 'grant') {
        await chatApi.grantCapability(request.contact_name, request.capability_id, {
          requestId: request.request_id,
          version: request.capability_version,
        })
        props.notify(t('reviews.capabilityGranted'))
      } else {
        await chatApi.denyCapability(request.contact_name, request.capability_id, request.request_id, request.capability_version)
        props.notify(t('reviews.capabilityDenied'))
      }
      setPage(1)
      refresh()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setCapabilityWorking(null)
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
      await api.decideReview(current.review.id, current.action, new Date(expiry()).toISOString())
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
    setExpiry(defaultExpiry())
    setDecision({ review, action })
  }
  const applicant = (item: Approval) =>
    displayIdentityName(item.kind === 'capability' ? item.contact_name : item.visitor)

  return (
    <>
      <header class="page-header">
        <h1>{t('reviews.title')}</h1>
        <button class="button button-secondary" type="button" onClick={refresh} disabled={approvals.loading}>
          {t('common.refresh')}
        </button>
      </header>

      <div class="segmented approval-tabs" role="group" aria-label={t('reviews.tabs')}>
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
                  <th>{t('reviews.request')}</th>
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
                        <span class="method">{t(item.kind === 'capability' ? 'reviews.capabilityTab' : 'reviews.accessTab')}</span>
                      </td>
                      <td class="approval-requester-cell" data-label={t('reviews.requester')}>
                        {item.kind === 'capability'
                          ? <button class="contact-link" type="button" title={t('contacts.openDetails')}
                              onClick={() => setSelectedContactName(item.contact_name)}>
                              <strong>{applicant(item)}</strong>
                            </button>
                          : <strong>{applicant(item)}</strong>}
                      </td>
                      <td class="approval-request-cell" data-label={t('reviews.request')}>
                        {item.kind === 'capability'
                          ? <strong>{t('contacts.chatCapability')}</strong>
                          : <div class="approval-target">
                              <div><span class="method">{item.method}</span> <code>{item.api}</code></div>
                            </div>}
                      </td>
                      <td class="approval-time-cell" data-label={t('reviews.requestedAt')}>{date(item.requested_at)}</td>
                      <td class="approval-expiry-cell" data-label={t('reviews.expiry')}>{date(item.expired_after)}</td>
                      <td class="approval-actions-cell" data-label={t('common.actions')}
                        aria-busy={tab() === 'pending' && item.kind === 'capability' && capabilityWorking() === item.request_id || tab() === 'expired' && deleting()}>
                        <Show when={tab() === 'pending'} fallback={
                          <div class="row-actions">
                            <button class="button button-danger-quiet button-small" type="button"
                              disabled={deleting()} onClick={() => setDeleteCandidate(item)}>
                              {t('common.delete')}
                            </button>
                          </div>
                        }>
                          {item.kind === 'capability'
                            ? <div class="row-actions">
                                <button class="button button-success button-small" type="button"
                                  disabled={capabilityWorking() !== null}
                                  onClick={() => void decideCapability(item, 'grant')}>
                                  {t('reviews.allow')}
                                </button>
                                <button class="button button-danger-quiet button-small" type="button"
                                  disabled={capabilityWorking() !== null}
                                  onClick={() => void decideCapability(item, 'deny')}>
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
          <span class="method">{decision()?.review.method}</span>
          <code>{decision()?.review.api}</code>
          <span>{displayIdentityName(decision()?.review.visitor ?? '')}</span>
        </div>
        <label class="field">
          <span>{t('reviews.decisionExpires')}</span>
          <input
            type="datetime-local"
            min={new Date().toISOString().slice(0, 16)}
            value={expiry()}
            onInput={(event) => setExpiry(event.currentTarget.value)}
            required
          />
        </label>
        <div class="dialog-actions">
          <button class="button button-secondary" type="button" onClick={() => setDecision(null)} disabled={submitting()}>
            {t('common.cancel')}
          </button>
          <button
            class={`button ${decision()?.action === 'allow' ? 'button-success' : 'button-danger'}`}
            type="button"
            onClick={decide}
            disabled={submitting() || !expiry()}
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
