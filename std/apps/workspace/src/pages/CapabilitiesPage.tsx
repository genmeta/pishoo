import { For, Show, createResource, createSignal } from 'solid-js'

import { api } from '../api/client'
import type { CapabilityDescriptor } from '../api/types'
import { Dialog, ErrorState, LoadingState } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { errorMessage } from '../lib/format'

export default function CapabilitiesPage() {
  const { t } = useI18n()
  const load = abortable((_source: boolean, signal: AbortSignal) => api.capabilities(signal))
  const [capabilities, actions] = createResource(() => true, load)
  const [selected, setSelected] = createSignal<CapabilityDescriptor>()

  return (
    <>
      <header class="page-header">
        <h1>{t('capabilities.title')}</h1>
        <button class="button button-secondary" type="button" onClick={() => void actions.refetch()} disabled={capabilities.loading}>
          {t('common.refresh')}
        </button>
      </header>
      <Show when={capabilities.error}>
        <section class="data-panel">
          <ErrorState message={errorMessage(capabilities.error, t('common.unexpectedError'))}
            onRetry={() => void actions.refetch()} />
        </section>
      </Show>
      <Show when={capabilities.loading && !capabilities()}>
        <section class="data-panel"><LoadingState label={t('capabilities.loading')} /></section>
      </Show>
      <Show when={!capabilities.error && capabilities()}>
        <section class="capability-grid" aria-label={t('capabilities.title')}>
          <Show when={capabilities()!.length > 0} fallback={
            <div class="data-panel"><p class="empty-state">{t('capabilities.empty')}</p></div>
          }>
            <For each={capabilities()}>{(capability) => <CapabilityCard capability={capability} onView={() => setSelected(capability)} />}</For>
          </Show>
        </section>
      </Show>
      <Dialog open={!!selected()} title={selected()?.title ?? ''} onClose={() => setSelected(undefined)}>
        <p class="muted">{t('apps.interfaces')} · {selected()?.endpoints.length ?? 0}</p>
        <ul class="capability-api-list">
          <For each={selected()?.endpoints}>{endpoint =>
            <li><span class="method">{endpoint.method}</span><code>{endpoint.path}</code></li>
          }</For>
        </ul>
      </Dialog>
    </>
  )
}

function CapabilityCard(props: { capability: CapabilityDescriptor; onView: () => void }) {
  const { t } = useI18n()
  const capability = () => props.capability
  return (
    <article class="capability-card">
      <header class="capability-card-header">
        <div>
          <h2>{capability().title}</h2>
        </div>
        <span class="status-badge">{t(capability().approval_mode === 'none' ? 'capabilities.noApproval' : 'capabilities.approvalRequired')}</span>
      </header>
      <Show when={capability().endpoints.some(endpoint => endpoint.method === 'POST' && endpoint.path === '/std/message')}>
        <p class="capability-summary">{t('capabilities.chat.summary')}</p>
      </Show>
      <footer class="capability-card-footer">
        <Show when={capability().selectable}><span>{t('capabilities.requestable')}</span></Show>
      </footer>
      <Show when={capability().endpoints.length > 0}>
        <button class="capability-endpoints" type="button" aria-haspopup="dialog" onClick={props.onView}>
          <span>{t('capabilities.endpoints', { count: capability().endpoints.length })}</span>
          <span aria-hidden="true">→</span>
        </button>
      </Show>
    </article>
  )
}
