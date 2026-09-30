import { For, Show, createResource } from 'solid-js'

import { api } from '../api/client'
import type { CapabilityDescriptor } from '../api/types'
import { ErrorState, LoadingState } from '../components/Ui'
import { capabilityLabel, useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { errorMessage } from '../lib/format'

export default function CapabilitiesPage() {
  const { t } = useI18n()
  const load = abortable((_source: boolean, signal: AbortSignal) => api.capabilities(signal))
  const [capabilities, actions] = createResource(() => true, load)

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
            <For each={capabilities()}>{(capability) => <CapabilityCard capability={capability} />}</For>
          </Show>
        </section>
      </Show>
    </>
  )
}

function CapabilityCard(props: { capability: CapabilityDescriptor }) {
  const { t } = useI18n()
  const capability = () => props.capability
  const id = () => capability().id
  return (
    <article class="capability-card">
      <header class="capability-card-header">
        <div>
          <code class="capability-kicker">{id()}</code>
          <h2>{capabilityLabel(t, id())}</h2>
        </div>
        <span class="status-badge">{t(capability().approval_mode === 'none' ? 'capabilities.noApproval' : 'capabilities.approvalRequired')}</span>
      </header>
      <p class="capability-summary">{capabilityLabel(t, id(), 'summary')}</p>
      <Show when={capability().endpoints.length > 0}>
        <details class="capability-endpoints">
          <summary>{t('capabilities.endpoints')}</summary>
          <ul>
            <For each={capability().endpoints}>{(endpoint) =>
              <li><span class="method">{endpoint.method}</span> <code>{endpoint.path}</code></li>
            }</For>
          </ul>
        </details>
      </Show>
      <footer class="capability-card-footer">
        <span>{t('capabilities.version', { version: capability().version })}</span>
        <Show when={capability().selectable}>
          <span>{t('capabilities.requestable')}</span>
        </Show>
      </footer>
    </article>
  )
}
