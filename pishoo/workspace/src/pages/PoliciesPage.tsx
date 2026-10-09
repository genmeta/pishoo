import { For, Show, createResource, createSignal } from 'solid-js'

import { api } from '../api/client'
import type { Effect, GrantedMethods, RuleRow } from '../api/types'
import { Dialog, EmptyState, ErrorState, LoadingState, Pagination, StatusBadge } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { displayIdentityName, errorMessage } from '../lib/format'

type View = 'api' | 'grantee'

const EFFECTS = ['allow', 'review', 'deny'] as const
const HTTP_METHODS = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS', 'CONNECT', 'TRACE', '*'] as const

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

export default function PoliciesPage(props: {
  notify: (message: string, tone?: 'success' | 'error') => void
}) {
  const { t } = useI18n()
  const requestedGrantee = new URLSearchParams(window.location.search).get('grantee')
  const [view, setView] = createSignal<View>(requestedGrantee ? 'grantee' : 'api')
  const [page, setPage] = createSignal(1)
  const [revision, setRevision] = createSignal(0)
  const [selection, setSelection] = createSignal<string | null>(requestedGrantee)
  const [editing, setEditing] = createSignal(false)
  const [editingRule, setEditingRule] = createSignal<RuleRow | null>(null)
  const [deleting, setDeleting] = createSignal<RuleRow | null>(null)
  const [submitting, setSubmitting] = createSignal(false)
  const [actionError, setActionError] = createSignal<string | null>(null)
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

  const openEditor = (rule?: RuleRow) => {
    setEditingRule(rule ?? null)
    setApiPath(rule?.api ?? (view() === 'api' ? currentSelection() ?? '' : ''))
    setMethod(rule?.method ?? 'GET')
    setEffect(rule?.effect ?? 'allow')
    setGrantee(rule?.grantee ?? (view() === 'grantee' ? currentSelection() ?? '' : ''))
    setActionError(null)
    setEditing(true)
  }

  const saveRule = async (event: SubmitEvent) => {
    event.preventDefault()
    if (submitting()) return
    const rule: RuleRow = {
      api: apiPath().trim().replace(/\/+$/, '') || '/',
      method: method(),
      effect: effect(),
      grantee: grantee().trim(),
    }
    setSubmitting(true)
    setActionError(null)
    try {
      await api.setRule(rule)
      if (view() === 'api') {
        const paths = [...new Set([...Object.keys(byApi() ?? {}), rule.api])].sort()
        setPage(Math.floor(paths.indexOf(rule.api) / 20) + 1)
      }
      setSelection(view() === 'api' ? rule.api : rule.grantee)
      setEditing(false)
      refresh()
      props.notify(t('policies.saved'))
    } catch (error) {
      setActionError(errorMessage(error, t('common.unexpectedError')))
    } finally {
      setSubmitting(false)
    }
  }

  const openDelete = (rule: RuleRow) => {
    setActionError(null)
    setDeleting(rule)
  }

  const deleteRule = async () => {
    const rule = deleting()
    if (!rule || submitting()) return
    setSubmitting(true)
    setActionError(null)
    try {
      await api.deleteRule(rule)
      if (view() === 'api' && rows().length === 1 && summaries()?.items.length === 1 && page() > 1) {
        setPage(page() - 1)
      }
      setDeleting(null)
      refresh()
      props.notify(t('policies.deleted'))
    } catch (error) {
      setActionError(errorMessage(error, t('common.unexpectedError')))
    } finally {
      setSubmitting(false)
    }
  }

  const loading = () => summaries.loading || byApi.loading || byGrantee.loading
  const loadError = () => summaries.error || byApi.error || byGrantee.error

  return (
    <div class="policies-page">
      <header class="page-header">
        <h1>{t('policies.title')}</h1>
        <div class="header-actions">
          <button class="button button-secondary" type="button" onClick={refresh} disabled={loading()}>
            {t('common.refresh')}
          </button>
          <button class="button button-primary" type="button" onClick={() => openEditor()} disabled={submitting()}>
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
                          <Show when={view() === 'api'}><td><code>{displayIdentityName(rule.grantee)}</code></td></Show>
                          <td>
                            <div class="row-actions">
                              <button class="button button-secondary button-small" type="button" disabled={submitting()} onClick={() => openEditor(rule)}>
                                {t('common.edit')}
                              </button>
                              <button class="button button-danger-quiet button-small" type="button" disabled={submitting()} onClick={() => openDelete(rule)}>
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

      <Dialog open={editing()} title={t('policies.title')} onClose={() => !submitting() && setEditing(false)}>
        <form class="form-grid" onSubmit={saveRule} aria-busy={submitting()}>
          <label class="field field-wide">
            <span>{t('policies.apiPath')}</span>
            <input value={apiPath()} onInput={(event) => setApiPath(event.currentTarget.value)}
              placeholder="/files/private" pattern="/.*" required disabled={submitting() || editingRule() !== null} />
          </label>
          <label class="field">
            <span id="rule-method-label">{t('policies.method')}</span>
            <select aria-labelledby="rule-method-label" value={method()} onChange={(event) => setMethod(event.currentTarget.value)} disabled={submitting() || editingRule() !== null}>
              <For each={HTTP_METHODS}>{(value) => <option value={value}>{value === '*' ? t('policies.anyMethod') : value}</option>}</For>
              <Show when={!HTTP_METHODS.some((value) => value === method())}>
                <option value={method()}>{method()}</option>
              </Show>
            </select>
          </label>
          <label class="field">
            <span id="rule-effect-label">{t('policies.effect')}</span>
            <select aria-labelledby="rule-effect-label" value={effect()} onChange={(event) => setEffect(event.currentTarget.value as Effect)} disabled={submitting()}>
              <For each={EFFECTS}>{(value) => <option value={value}>{t(`status.${value}`)}</option>}</For>
            </select>
          </label>
          <label class="field field-wide">
            <span>{t('policies.grantee')}</span>
            <input value={grantee()} onInput={(event) => setGrantee(event.currentTarget.value)}
              placeholder="alice.example.dhttp.net" list="grantee-options" aria-describedby="grantee-hint"
              required disabled={submitting() || editingRule() !== null} />
            <datalist id="grantee-options">
              <option value="**" /><option value="*?" /><option value="?" />
              <For each={grantees()}>{(name) => <option value={name} />}</For>
            </datalist>
          </label>
          <p id="grantee-hint" class="muted field-wide">{t('policies.granteeHint')}</p>
          <Show when={editingRule()}><p class="muted field-wide">{t('policies.editHint')}</p></Show>
          <Show when={actionError()}><p class="field-error field-wide" role="alert">{actionError()}</p></Show>
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
          <code>{deleting()?.api}</code><span class="method">{deleting()?.method}</span>
          <StatusBadge value={deleting()?.effect ?? 'deny'} /><code>{deleting()?.grantee}</code>
        </div>
        <p class="dialog-copy">{t('policies.deleteHint')}</p>
        <Show when={actionError()}><p class="field-error" role="alert">{actionError()}</p></Show>
        <div class="dialog-actions">
          <button class="button button-secondary" type="button" disabled={submitting()} onClick={() => setDeleting(null)}>
            {t('common.cancel')}
          </button>
          <button class="button button-danger" type="button" disabled={submitting()} onClick={() => void deleteRule()}>
            {submitting() ? t('common.deleting') : t('policies.deleteRule')}
          </button>
        </div>
      </Dialog>
    </div>
  )
}
