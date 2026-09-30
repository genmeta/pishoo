import { For, Show, createResource, createSignal } from 'solid-js'

import { api } from '../api/client'
import type { Effect, GrantedMethods, RuleRow } from '../api/types'
import { EmptyState, ErrorState, LoadingState, Pagination, StatusBadge } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { displayIdentityName, errorMessage } from '../lib/format'

type View = 'api' | 'grantee'

const EFFECTS = ['allow', 'review', 'deny'] as const
function flattenRules(
  buckets: Record<string, GrantedMethods> | undefined,
  toRow: (key: string, effect: Effect, value: string) => RuleRow,
): RuleRow[] {
  return Object.entries(buckets ?? {}).flatMap(([key, effects]) =>
    EFFECTS.flatMap((effect) => effects[effect].map((value) => toRow(key, effect, value))),
  )
}

function rowsForApi(
  apiPath: string,
  methods: Record<string, GrantedMethods> | undefined,
): RuleRow[] {
  return flattenRules(methods, (method, effect, grantee) => ({
    api: apiPath,
    method,
    effect,
    grantee,
  }))
}

function rowsForGrantee(
  grantee: string,
  paths: Record<string, GrantedMethods> | undefined,
): RuleRow[] {
  return flattenRules(paths, (api, effect, method) => ({ api, method, effect, grantee }))
}

export default function PoliciesPage() {
  const { t } = useI18n()
  const requestedGrantee = new URLSearchParams(window.location.search).get('grantee')
  const [view, setView] = createSignal<View>(requestedGrantee ? 'grantee' : 'api')
  const [page, setPage] = createSignal(1)
  const [revision, setRevision] = createSignal(0)
  const [selection, setSelection] = createSignal<string | null>(requestedGrantee)

  const loadSummaries = abortable(
    ([currentPage]: readonly [number, number], signal: AbortSignal) =>
      api.apiSummaries(currentPage, signal),
  )
  const loadRulesByApi = abortable((_revision: number, signal: AbortSignal) =>
    api.rulesByApi(signal),
  )
  const loadRulesByGrantee = abortable((_revision: number, signal: AbortSignal) =>
    api.rulesByGrantee(signal),
  )
  const [summaries] = createResource(
    () => [page(), revision()] as const,
    loadSummaries,
  )
  const [byApi] = createResource(revision, loadRulesByApi)
  const [byGrantee] = createResource(revision, loadRulesByGrantee)

  const refresh = () => setRevision((value) => value + 1)
  const grantees = () => Object.keys(byGrantee() ?? {}).sort()
  const currentSelection = () => {
    const selected = selection()
    const options = view() === 'api' ? summaries()?.items.map((item) => item.api) ?? [] : grantees()
    return selected && options.includes(selected) ? selected : options[0] ?? selected
  }
  const selectionLabel = () => {
    const selected = currentSelection()
    if (!selected) return t('policies.noSelection')
    return view() === 'grantee' ? displayIdentityName(selected) : selected
  }
  const rows = () => {
    const selected = currentSelection()
    if (!selected) return []
    return view() === 'api'
      ? rowsForApi(selected, byApi()?.[selected])
      : rowsForGrantee(selected, byGrantee()?.[selected])
  }

  const changeView = (next: View) => {
    setView(next)
    setSelection(null)
    setPage(1)
  }

  const loading = () => summaries.loading || byApi.loading || byGrantee.loading
  const loadError = () => summaries.error || byApi.error || byGrantee.error

  return (
    <div class="policies-page">
      <header class="page-header">
        <h1>{t('policies.title')}</h1>
        <button class="button button-secondary" type="button" onClick={refresh} disabled={loading()}>
          {t('common.refresh')}
        </button>
      </header>
      <div class="toolbar">
        <div class="segmented" role="tablist" aria-label={t('policies.organization')}>
          <button
            type="button"
            role="tab"
            aria-selected={view() === 'api'}
            classList={{ active: view() === 'api' }}
            onClick={() => changeView('api')}
          >
            {t('policies.byApi')}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={view() === 'grantee'}
            classList={{ active: view() === 'grantee' }}
            onClick={() => changeView('grantee')}
          >
            {t('policies.byGrantee')}
          </button>
        </div>
        <span class="toolbar-meta">{t('policies.explicitCount', { count: rows().length })}</span>
      </div>

      <Show when={loadError()}>
        <section class="data-panel">
          <ErrorState message={errorMessage(loadError(), t('common.unexpectedError'))} onRetry={refresh} />
        </section>
      </Show>
      <Show when={loading() && !byApi() && !byGrantee()}>
        <section class="data-panel"><LoadingState label={t('policies.loading')} /></section>
      </Show>
      <Show when={!loadError() && (byApi() || byGrantee())}>
        <div class="master-detail">
          <aside class="master-list" aria-label={view() === 'api' ? t('policies.apis') : t('policies.grantees')}>
            <div class="master-list-header">
              <strong>{view() === 'api' ? t('policies.apis') : t('policies.grantees')}</strong>
              <span>{view() === 'api' ? summaries()?.total ?? 0 : grantees().length}</span>
            </div>
            <div class="master-list-items">
              <Show
                when={(view() === 'api' ? summaries()?.items.length : grantees().length) ?? 0}
                fallback={<EmptyState title={view() === 'api' ? t('policies.noApis') : t('policies.noGrantees')} />}
              >
                <For each={view() === 'api' ? summaries()?.items.map((item) => item.api) : grantees()}>
                  {(item) => (
                    <button
                      type="button"
                      classList={{ active: currentSelection() === item }}
                      onClick={() => setSelection(item)}
                    >
                      <code>{view() === 'grantee' ? displayIdentityName(item) : item}</code>
                      <span>›</span>
                    </button>
                  )}
                </For>
              </Show>
            </div>
            <Show when={view() === 'api' && (summaries()?.total ?? 0) > 20}>
              <Pagination
                page={summaries()?.page ?? page()}
                pageSize={summaries()?.page_size ?? 20}
                total={summaries()?.total ?? 0}
                onPage={(next) => {
                  setPage(next)
                  setSelection(null)
                }}
              />
            </Show>
          </aside>

          <section class="data-panel rule-detail" aria-label={t('policies.selectedRules')}>
            <header class="section-header">
              <div>
                <span>{view() === 'api' ? t('policies.api') : t('policies.grantee')}</span>
                <h2><code>{selectionLabel()}</code></h2>
              </div>
            </header>
            <Show when={rows().length} fallback={<EmptyState title={t('policies.noExplicit')} />}>
              <div class="table-scroll">
                <table>
                  <thead>
                    <tr>
                      <Show when={view() === 'grantee'}><th>{t('policies.api')}</th></Show>
                      <th>{t('policies.method')}</th>
                      <th>{t('policies.effect')}</th>
                      <Show when={view() === 'api'}><th>{t('policies.grantee')}</th></Show>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={rows()}>
                      {(rule) => (
                        <tr>
                          <Show when={view() === 'grantee'}><td><code>{rule.api}</code></td></Show>
                          <td><span class="method">{rule.method}</span></td>
                          <td><StatusBadge value={rule.effect} /></td>
                          <Show when={view() === 'api'}><td><code>{displayIdentityName(rule.grantee)}</code></td></Show>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
            </Show>
          </section>
        </div>
      </Show>

    </div>
  )
}
