import { For, Show, createResource, createSignal, onCleanup, onMount } from 'solid-js'

import { ApiError, api } from '../api/client'
import type { Review } from '../api/types'
import { Dialog, EmptyState, ErrorState, LoadingState, Pagination, StatusBadge } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { errorMessage } from '../lib/format'

interface Decision {
  review: Review
  action: 'allow' | 'deny'
}

function defaultExpiry(): string {
  const date = new Date(Date.now() + 60 * 60 * 1000)
  const local = new Date(date.getTime() - date.getTimezoneOffset() * 60_000)
  return local.toISOString().slice(0, 16)
}

export default function ReviewsPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
}) {
  const { date, t } = useI18n()
  const [page, setPage] = createSignal(1)
  const [revision, setRevision] = createSignal(0)
  const [decision, setDecision] = createSignal<Decision | null>(null)
  const [expiry, setExpiry] = createSignal(defaultExpiry())
  const [submitting, setSubmitting] = createSignal(false)
  const loadReviews = abortable((currentPage: number, signal: AbortSignal) =>
    api.reviews(currentPage, signal),
  )
  const [reviews] = createResource(() => [page(), revision()] as const, ([currentPage]) =>
    loadReviews(currentPage),
  )

  const refresh = () => setRevision((value) => value + 1)
  const decisionAction = () =>
    decision()?.action === 'allow' ? t('reviews.allow') : t('reviews.deny')
  const submitLabel = () => {
    if (submitting()) return t('reviews.submitting')
    return decision()?.action === 'allow' ? t('reviews.allowRequest') : t('reviews.denyRequest')
  }

  const decide = async () => {
    const current = decision()
    if (!current) return
    setSubmitting(true)
    try {
      await api.decideReview(current.review.id, current.action, new Date(expiry()).toISOString())
      props.notify(current.action === 'allow' ? t('reviews.allowed') : t('reviews.denied'))
      setDecision(null)
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

  let polling: number | undefined
  onMount(() => {
    polling = window.setInterval(() => {
      if (!document.hidden) refresh()
    }, 5000)
  })
  onCleanup(() => window.clearInterval(polling))

  return (
    <>
      <header class="page-header">
        <div>
          <p class="eyebrow">{t('reviews.eyebrow')}</p>
          <h1>{t('reviews.title')}</h1>
        </div>
        <button class="button button-secondary" type="button" onClick={refresh} disabled={reviews.loading}>
          {t('common.refresh')}
        </button>
      </header>

      <div class="toolbar">
        <span class="toolbar-meta" aria-live="polite">
          {reviews.loading
            ? t('reviews.updating')
            : t('reviews.pendingCount', { count: reviews()?.total ?? 0 })}
        </span>
      </div>

      <section class="data-panel" aria-label={t('reviews.section')}>
        <Show when={reviews.error}>
          <ErrorState message={errorMessage(reviews.error, t('common.unexpectedError'))} onRetry={refresh} />
        </Show>
        <Show when={reviews.loading && !reviews()}>
          <LoadingState label={t('reviews.loading')} />
        </Show>
        <Show when={!reviews.error && reviews()?.items.length === 0}>
          <EmptyState title={t('reviews.clear')} detail={t('reviews.noneWaiting')} />
        </Show>
        <Show when={reviews()?.items.length}>
          <div class="table-scroll">
            <table>
              <thead>
                <tr>
                  <th>{t('reviews.request')}</th>
                  <th>{t('reviews.visitor')}</th>
                  <th>{t('reviews.target')}</th>
                  <th>{t('reviews.reason')}</th>
                  <th>{t('reviews.expiry')}</th>
                  <th><span class="sr-only">{t('common.actions')}</span></th>
                </tr>
              </thead>
              <tbody>
                <For each={reviews()?.items}>
                  {(review) => (
                    <tr>
                      <td>
                        <div class="primary-cell">
                          <strong>#{review.id}</strong>
                          <StatusBadge value="pending" />
                        </div>
                      </td>
                      <td>{review.visitor}</td>
                      <td>
                        <span class="method">{review.method}</span>
                        <code>{review.api}</code>
                      </td>
                      <td class="reason-cell">{review.reason}</td>
                      <td>{date(review.expired_after)}</td>
                      <td>
                        <div class="row-actions">
                          <button
                            class="button button-success button-small"
                            type="button"
                            onClick={() => {
                              setExpiry(defaultExpiry())
                              setDecision({ review, action: 'allow' })
                            }}
                          >
                            {t('reviews.allow')}
                          </button>
                          <button
                            class="button button-danger-quiet button-small"
                            type="button"
                            onClick={() => {
                              setExpiry(defaultExpiry())
                              setDecision({ review, action: 'deny' })
                            }}
                          >
                            {t('reviews.deny')}
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
            page={reviews()?.page ?? page()}
            pageSize={reviews()?.page_size ?? 20}
            total={reviews()?.total ?? 0}
            onPage={setPage}
          />
        </Show>
      </section>

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
          <span>{decision()?.review.visitor}</span>
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
    </>
  )
}
