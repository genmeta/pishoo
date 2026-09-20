import { For, Show, createResource, createSignal } from 'solid-js'

import { api } from '../api/client'
import type { Effect, GrantedMethods, RuleRow } from '../api/types'
import { Dialog, EmptyState, ErrorState, LoadingState, Pagination, StatusBadge } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { errorMessage } from '../lib/format'

type View = 'api' | 'grantee'

const EFFECTS = ['allow', 'review', 'deny'] as const
const HTTP_METHODS = [
  'GET',
  'POST',
  'PUT',
  'PATCH',
  'DELETE',
  'HEAD',
  'OPTIONS',
  'CONNECT',
  'TRACE',
] as const

function isKnownHttpMethod(method: string): boolean {
  return method === '*' || HTTP_METHODS.some((value) => value === method)
}

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
  methods: Record<string, { allow: string[]; review: string[]; deny: string[] }> | undefined,
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

export default function PoliciesPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
}) {
  const { t } = useI18n()
  const [view, setView] = createSignal<View>('api')
  const [page, setPage] = createSignal(1)
  const [revision, setRevision] = createSignal(0)
  const [selection, setSelection] = createSignal<string | null>(null)
  const [editing, setEditing] = createSignal(false)
  const [deleting, setDeleting] = createSignal<RuleRow | null>(null)
  const [submitting, setSubmitting] = createSignal(false)
  const [apiPath, setApiPath] = createSignal('')
  const [method, setMethod] = createSignal('GET')
  const [effect, setEffect] = createSignal<Effect>('allow')
  const [grantee, setGrantee] = createSignal('')

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
    return selected && options.includes(selected) ? selected : options[0] ?? null
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

  const openEditor = (rule?: RuleRow) => {
    const selectedApi = view() === 'api' ? currentSelection() ?? '' : ''
    const selectedGrantee = view() === 'grantee' ? currentSelection() ?? '' : ''
    setApiPath(rule?.api ?? selectedApi)
    setMethod(rule?.method ?? 'GET')
    setEffect(rule?.effect ?? 'allow')
    setGrantee(rule?.grantee ?? selectedGrantee)
    setEditing(true)
  }

  const saveRule = async (event: SubmitEvent) => {
    event.preventDefault()
    setSubmitting(true)
    try {
      await api.setRule({
        api: apiPath().trim(),
        method: method(),
        effect: effect(),
        grantee: grantee().trim(),
      })
      props.notify(t('policies.saved'))
      setEditing(false)
      setSelection(view() === 'api' ? apiPath().trim() : grantee().trim())
      refresh()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setSubmitting(false)
    }
  }

  const deleteRule = async () => {
    const rule = deleting()
    if (!rule) return
    setSubmitting(true)
    try {
      await api.deleteRule(rule)
      props.notify(t('policies.deleted'))
      setDeleting(null)
      refresh()
    } catch (error) {
      props.notify(errorMessage(error, t('common.unexpectedError')), 'error')
    } finally {
      setSubmitting(false)
    }
  }

  const loading = () => summaries.loading || byApi.loading || byGrantee.loading
  const loadError = () => summaries.error || byApi.error || byGrantee.error

  return (
    <div class="policies-page">
      <header class="page-header">
        <div>
          <p class="eyebrow">{t('policies.eyebrow')}</p>
          <h1>{t('policies.title')}</h1>
        </div>
        <div class="header-actions">
          <button class="button button-secondary" type="button" onClick={refresh} disabled={loading()}>
            {t('common.refresh')}
          </button>
          <button class="button button-primary" type="button" onClick={() => openEditor()}>
            {t('policies.add')}
          </button>
        </div>
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
                      <code>{item}</code>
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
                <h2><code>{currentSelection() ?? t('policies.noSelection')}</code></h2>
              </div>
              <button class="button button-primary button-small" type="button" onClick={() => openEditor()}>
                {t('policies.add')}
              </button>
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
                      <th><span class="sr-only">{t('common.actions')}</span></th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={rows()}>
                      {(rule) => (
                        <tr>
                          <Show when={view() === 'grantee'}><td><code>{rule.api}</code></td></Show>
                          <td><span class="method">{rule.method}</span></td>
                          <td><StatusBadge value={rule.effect} /></td>
                          <Show when={view() === 'api'}><td><code>{rule.grantee}</code></td></Show>
                          <td>
                            <div class="row-actions">
                              <button class="button button-secondary button-small" type="button" onClick={() => openEditor(rule)}>
                                {t('common.edit')}
                              </button>
                              <button class="button button-danger-quiet button-small" type="button" onClick={() => setDeleting(rule)}>
                                {t('common.delete')}
                              </button>
                            </div>
                          </td>
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

      <Dialog open={editing()} title={t('policies.editorTitle')} onClose={() => !submitting() && setEditing(false)}>
        <form class="form-grid" onSubmit={saveRule}>
          <label class="field field-wide">
            <span>{t('policies.apiPath')}</span>
            <input
              value={apiPath()}
              onInput={(event) => setApiPath(event.currentTarget.value)}
              placeholder="/files/private"
              pattern="/.*"
              required
            />
          </label>
          <label class="field">
            <span>{t('policies.method')}</span>
            <select
              value={method()}
              onChange={(event) => setMethod(event.currentTarget.value)}
            >
              <For each={HTTP_METHODS}>{(value) => <option value={value}>{value}</option>}</For>
              <option value="*">{t('policies.anyMethod')}</option>
              <Show when={!isKnownHttpMethod(method())}>
                <option value={method()}>{method()}</option>
              </Show>
            </select>
          </label>
          <label class="field">
            <span>{t('policies.effect')}</span>
            <select value={effect()} onChange={(event) => setEffect(event.currentTarget.value as Effect)}>
              <option value="allow">{t('status.allow')}</option>
              <option value="review">{t('status.review')}</option>
              <option value="deny">{t('status.deny')}</option>
            </select>
          </label>
          <label class="field field-wide">
            <span>{t('policies.grantee')}</span>
            <input
              value={grantee()}
              onInput={(event) => setGrantee(event.currentTarget.value)}
              placeholder="alice.example"
              list="grantee-options"
              required
            />
            <datalist id="grantee-options">
              <option value="**" />
              <option value="*?" />
              <option value="?" />
              <For each={grantees()}>{(name) => <option value={name} />}</For>
            </datalist>
          </label>
          <div class="dialog-actions field-wide">
            <button class="button button-secondary" type="button" disabled={submitting()} onClick={() => setEditing(false)}>
              {t('common.cancel')}
            </button>
            <button class="button button-primary" type="submit" disabled={submitting()}>
              {submitting() ? t('common.saving') : t('policies.save')}
            </button>
          </div>
        </form>
      </Dialog>

      <Dialog open={deleting() !== null} title={t('policies.deleteTitle')} onClose={() => !submitting() && setDeleting(null)}>
        <div class="decision-summary">
          <code>{deleting()?.api}</code>
          <span class="method">{deleting()?.method}</span>
          <StatusBadge value={deleting()?.effect ?? 'deny'} />
          <code>{deleting()?.grantee}</code>
        </div>
        <p class="dialog-copy">{t('policies.lockoutWarning')}</p>
        <div class="dialog-actions">
          <button class="button button-secondary" type="button" disabled={submitting()} onClick={() => setDeleting(null)}>
            {t('common.cancel')}
          </button>
          <button class="button button-danger" type="button" disabled={submitting()} onClick={deleteRule}>
            {submitting() ? t('common.deleting') : t('policies.deleteRule')}
          </button>
        </div>
      </Dialog>
    </div>
  )
}
