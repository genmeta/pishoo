import type { JSX } from 'solid-js'
import { Show, createEffect, createSignal, on, onCleanup, onMount } from 'solid-js'

import { statusLabel, useI18n } from '../i18n'
import { initials } from '../lib/format'

export function Avatar(props: { name: string; src?: string | null; large?: boolean }) {
  const [failed, setFailed] = createSignal(false)
  createEffect(on(() => props.src, () => setFailed(false)))
  return (
    <span class="avatar" classList={{ 'avatar-large': props.large }} aria-hidden="true">
      <Show when={props.src && !failed()} fallback={initials(props.name)}>
        <img src={props.src!} alt="" onError={() => setFailed(true)} />
      </Show>
    </span>
  )
}

export function StatusBadge(props: { value: string }) {
  const { t } = useI18n()
  return <span class={`status status-${props.value}`}>{statusLabel(t, props.value)}</span>
}

export function LoadingState(props: { label?: string }) {
  const { t } = useI18n()
  return (
    <div class="state-block" role="status">
      <span class="spinner" aria-hidden="true" />
      <span>{props.label ?? t('common.loading')}</span>
    </div>
  )
}

export function EmptyState(props: { title: string; detail?: string }) {
  return (
    <div class="state-block state-empty">
      <strong>{props.title}</strong>
      <Show when={props.detail}>
        <span>{props.detail}</span>
      </Show>
    </div>
  )
}

export function ErrorState(props: { message: string; onRetry?: () => void }) {
  const { t } = useI18n()
  return (
    <div class="state-block state-error" role="alert">
      <strong>{t('common.couldNotLoad')}</strong>
      <span>{props.message}</span>
      <Show when={props.onRetry}>
        <button class="button button-secondary" type="button" onClick={props.onRetry}>
          {t('common.retry')}
        </button>
      </Show>
    </div>
  )
}

export function Pagination(props: {
  page: number
  pageSize: number
  total: number
  onPage: (page: number) => void
}) {
  const { t } = useI18n()
  const pages = () => Math.max(1, Math.ceil(props.total / props.pageSize))
  return (
    <div class="pagination" aria-label={t('common.pagination')}>
      <span>{t('common.pageSummary', { page: props.page, pages: pages(), total: props.total })}</span>
      <div class="button-group">
        <button
          class="icon-button"
          type="button"
          aria-label={t('common.previousPage')}
          title={t('common.previousPage')}
          disabled={props.page <= 1}
          onClick={() => props.onPage(props.page - 1)}
        >
          ‹
        </button>
        <button
          class="icon-button"
          type="button"
          aria-label={t('common.nextPage')}
          title={t('common.nextPage')}
          disabled={props.page >= pages()}
          onClick={() => props.onPage(props.page + 1)}
        >
          ›
        </button>
      </div>
    </div>
  )
}

export function Dialog(props: {
  open: boolean
  title: string
  children: JSX.Element
  onClose: () => void
  wide?: boolean
}) {
  const { t } = useI18n()
  let dialog: HTMLElement | undefined
  let previouslyFocused: HTMLElement | null = null
  let wasOpen = false

  // A modal owns keyboard focus until it returns focus to its trigger.
  onMount(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!props.open || !dialog) return
      if (event.key === 'Escape') {
        props.onClose()
        return
      }
      if (event.key !== 'Tab') return

      const focusable = Array.from(dialog.querySelectorAll<HTMLElement>(
        'button, input, select, textarea, a[href], [tabindex]:not([tabindex="-1"])',
      )).filter((element) => !element.hasAttribute('disabled'))
      if (!focusable.length) return

      const first = focusable[0]
      const last = focusable[focusable.length - 1]
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault()
        last.focus()
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault()
        first.focus()
      }
    }
    document.addEventListener('keydown', onKeyDown)
    onCleanup(() => document.removeEventListener('keydown', onKeyDown))
  })

  createEffect(() => {
    if (props.open && !wasOpen) {
      previouslyFocused = document.activeElement instanceof HTMLElement ? document.activeElement : null
      queueMicrotask(() => dialog?.focus())
    } else if (!props.open && wasOpen) {
      queueMicrotask(() => previouslyFocused?.focus())
      previouslyFocused = null
    }
    wasOpen = props.open
  })

  return (
    <Show when={props.open}>
      <div class="dialog-backdrop" onMouseDown={(event) => event.target === event.currentTarget && props.onClose()}>
        <section
          ref={dialog}
          class="dialog"
          classList={{ 'dialog-wide': props.wide }}
          role="dialog"
          aria-modal="true"
          aria-label={props.title}
          tabIndex={-1}
        >
          <header class="dialog-header">
            <h2>{props.title}</h2>
            <button
              class="icon-button"
              type="button"
              aria-label={t('common.closeDialog')}
              title={t('common.close')}
              onClick={props.onClose}
            >
              ×
            </button>
          </header>
          <div class="dialog-body">{props.children}</div>
        </section>
      </div>
    </Show>
  )
}

export function Toast(props: {
  message: string | null
  tone: 'success' | 'error'
  onClose: () => void
}) {
  const { t } = useI18n()
  return (
    <Show when={props.message}>
      <div class={`toast toast-${props.tone}`} role={props.tone === 'error' ? 'alert' : 'status'}>
        <span>{props.message}</span>
        <button class="icon-button" type="button" aria-label={t('common.dismissMessage')} onClick={props.onClose}>
          ×
        </button>
      </div>
    </Show>
  )
}
