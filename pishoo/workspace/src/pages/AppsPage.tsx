import { For, Show, createResource } from 'solid-js'

import { api } from '../api/client'
import { ErrorState, LoadingState } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { errorMessage } from '../lib/format'

export default function AppsPage() {
  const { t } = useI18n()
  const load = abortable((_source: boolean, signal: AbortSignal) => api.libs(signal))
  const [libs, actions] = createResource(() => true, load)

  return (
    <>
      <header class="page-header">
        <h1>{t('nav.apps')}</h1>
        <button class="button button-secondary" type="button" disabled={libs.loading}
          onClick={() => void actions.refetch()}>{t('common.refresh')}</button>
      </header>
      <Show when={libs.error}>
        <section class="data-panel">
          <ErrorState message={errorMessage(libs.error, t('common.unexpectedError'))}
            onRetry={() => void actions.refetch()} />
        </section>
      </Show>
      <Show when={libs.loading && !libs()}>
        <section class="data-panel"><LoadingState label={t('apps.loading')} /></section>
      </Show>
      <Show when={!libs.error && libs()}>
        <Show when={libs()!.length > 0} fallback={
          <section class="overview-panel placeholder-panel" role="status">
            <h2>{t('apps.empty')}</h2>
          </section>
        }>
          <section class="capability-grid" aria-label={t('apps.wasm')}>
            <For each={libs()}>{(lib) => (
              <article class="capability-card">
                <header class="capability-card-header">
                  <div>
                    <code class="capability-kicker">{lib.id}</code>
                    <h2>{lib.title || lib.id}</h2>
                  </div>
                  <span class="status-badge">WASM</span>
                </header>
                <Show when={lib.description}><p class="capability-summary">{lib.description}</p></Show>
                <details class="capability-endpoints">
                  <summary>{t('apps.endpoints', { count: lib.endpoints.length })}</summary>
                  <ul><For each={lib.endpoints}>{(endpoint) => (
                    <li>
                      <span class="method">{endpoint.method}</span>{' '}
                      <Show when={endpoint.method === 'GET'} fallback={<code>{endpoint.path}</code>}>
                        <a class="wasm-api-link" href={endpoint.path} target="_blank" rel="noopener"
                          aria-label={t('apps.openApi', { path: endpoint.path })}>
                          <code>{endpoint.path}</code> <span aria-hidden="true">↗</span>
                        </a>
                      </Show>
                      <Show when={endpoint.description}>
                        <span class="wasm-api-description">{endpoint.description}</span>
                      </Show>
                    </li>
                  )}</For></ul>
                </details>
                <footer class="capability-card-footer">
                  <span>{t('capabilities.version', { version: lib.version })}</span>
                </footer>
              </article>
            )}</For>
          </section>
        </Show>
      </Show>
    </>
  )
}
