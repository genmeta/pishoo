import {
  For,
  Show,
  createEffect,
  createResource,
  createSignal,
  on,
  onCleanup,
  onMount,
} from 'solid-js'

import { api, request } from '../api/client'
import type { LibSummary, StoreLib, StoreCatalog, StoreDetail, StoreVersions, StoreUpdate } from '../api/types'
import {
  Dialog,
  EmptyState,
  ErrorState,
  LoadingState,
  Pagination,
} from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { errorMessage } from '../lib/format'
import { workspacePath } from '../lib/routes'
import './apps.css'

function validId(id: string) {
  const parts = id.split('/')
  const short = /^[a-z][a-z0-9-]{0,62}$/
  return parts.length === 1
    ? short.test(id)
    : parts.length === 2 &&
        parts[0].length <= 253 && parts[0].split('.').every(label => label.length <= 63) &&
        /^(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+dhttp\.net$/.test(parts[0]) &&
        short.test(parts[1])
}

const storeBase = '/std/dhttp/store.dhttp.net/store/v1'

function validVersion(version: string) {
  return /^(0|[1-9][0-9]{0,19})\.(0|[1-9][0-9]{0,19})\.(0|[1-9][0-9]{0,19})$/.test(version)
    && version.split('.').every(part => BigInt(part) <= 18446744073709551615n)
}

async function storeRead<T>(path: string, signal?: AbortSignal) {
  return request<T>(storeBase + path, {
    signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(30_000)]) : AbortSignal.timeout(30_000),
  })
}

async function readStoreDetail(id: string, version: string, signal?: AbortSignal, kind: 'app' | 'lib' = 'lib') {
  if (!validId(id) || !id.includes('/') || version && !validVersion(version))
    throw new Error('Invalid store release identity')
  const result = await storeRead<StoreDetail>(`/${kind === 'app' ? 'apps' : 'libs'}/${id}${version ? `/versions/${version}` : ''}`, signal)
  if (result.id !== id || result.publisher + '/' + result.name !== id
    || !validVersion(result.version) || version && result.version !== version
    || result.kind !== kind || !result.title || !result.description || !result.changelog
    || !Array.isArray(result.dependencies)
    || (kind === 'lib' && (!result.openapi?.info || result.openapi.info.version !== result.version))
    || (kind === 'app' && result.openapi !== undefined)
    || !['published', 'yanked'].includes(result.status))
    throw new Error('Store release metadata does not match the requested identity')
  return result
}

function newerVersion(next: string, previous: string) {
  const a = next.split('.').map(BigInt), b = previous.split('.').map(BigInt)
  for (let index = 0; index < 3; index++) {
    if (a[index] !== b[index]) return a[index] > b[index]
  }
  return false
}

function storeManifest(release: StoreDetail | undefined): LibSummary | undefined {
  if (!release) return undefined
  const endpoints: LibSummary['endpoints'] = []
  for (const [path, item] of Object.entries(release.openapi?.paths ?? {})) {
    for (const [method, op] of Object.entries(item)) {
      if (['get', 'post', 'put', 'patch', 'delete', 'head', 'options', 'trace'].includes(method) && op)
        endpoints.push({ method: method.toUpperCase(), path: `/std${path}`, description: op.summary ?? op.description ?? null })
    }
  }
  return { kind: release.kind, dependencies: release.dependencies, id: release.id, title: release.title, version: release.version,
    description: release.description, repository: release.repository, changelog: release.changelog ?? undefined, host: release.host ?? undefined,
    entry: release.kind === 'app' ? '/' : undefined, endpoints, openapi: release.openapi }
}

function operations(lib: LibSummary | undefined) {
  return (lib?.endpoints ?? [])
    .map((endpoint) => {
      const path = endpoint.path.replace(/^\/std(?=\/|$)/, '')
      const method = endpoint.method.toLowerCase() as keyof NonNullable<
        NonNullable<LibSummary['openapi']>['paths']
      >[string]
      const operation = lib?.openapi?.paths?.[path]?.[method]
      return { ...endpoint, operation }
    })
    .sort(
      (a, b) =>
        a.method.localeCompare(b.method) || a.path.localeCompare(b.path),
    )
}

function pageResource(op: ReturnType<typeof operations>[number]) {
  const extension = op.operation?.['x-pishoo']
  return extension && 'static-file' in extension && typeof extension['static-file'] === 'string'
    ? extension['static-file'] : undefined
}

function repositoryUrl(value: string | undefined) {
  if (!value || !/^https:\/\//i.test(value) || /[\s\\]/.test(value)) return undefined
  try {
    const url = new URL(value)
    return url.protocol === 'https:' && url.hostname && !url.username && !url.password
      ? url.href : undefined
  } catch {
    return undefined
  }
}

function entry(lib: LibSummary | undefined) {
  const path = lib?.entry
  if (
    !lib ||
    !validId(lib.id) ||
    !path ||
    !path.startsWith('/') ||
    /[\\?#%]/.test(path) ||
    path.split('/').some((part) => part === '.' || part === '..')
  )
    return undefined
  const target = `/${lib.id.split('/').at(-1)}${path}`
  return lib.kind === 'app' ? target : undefined
}

export default function AppsPage() {
  const { t } = useI18n()
  const locationUrl = () => {
    const value = new URL(window.location.href)
    value.pathname = value.pathname.replace(/^\/workspace(?=\/|$)/, '') || '/'
    return value
  }
  const [url, setUrl] = createSignal(locationUrl())
  const params = () => url().searchParams
  const kind = () => params().get('kind') === 'lib' ? 'lib' as const : 'app' as const
  const collection = () => kind() === 'app' ? 'apps' : 'libs'
  const tab = () => params().get('tab') === 'store' ? 'store' : 'mine'
  const detail = () => !['/apps', '/apps/'].includes(url().pathname)
  const section = () => {
    const value = params().get('section') ?? ''
    if (value === 'trust' && identity()?.source !== 'store') return 'about'
    return ['changes', 'interfaces', 'access', 'trust'].includes(value) ? value : 'about'
  }
  const [installed, installedActions] = createResource(kind, abortable((kind: 'app' | 'lib', signal) =>
    request<LibSummary[]>(`/std/sys/${kind}/list`, { signal }),
  ))
  const installedItems = () => installed.error ? [] : installed.latest?.filter(item => (item.kind ?? 'lib') === kind()) ?? []
  const saved = (id: string) => installedItems().find(item => item.id === id)
  const identity = () => {
    const match = /^\/apps\/(local|store)\/(.+?)\/?$/.exec(url().pathname)
    if (!match) return null
    try {
      const parts = match[2].split('/').map(decodeURIComponent)
      if (parts.some(part => part.includes('/'))) return null
      if (match[1] === 'store') {
        if (parts.length !== 2) return null
        if (!parts[0].endsWith('.dhttp.net')) parts[0] += '.dhttp.net'
        return validId(parts.join('/')) ? { source: 'store', id: parts.join('/') } : null
      }
      if (parts.length !== 1 || !validId(parts[0])) return null
      return { source: 'local', id: installedItems().find(item => item.id.split('/').at(-1) === parts[0])?.id ?? parts[0] }
    } catch { return null }
  }
  const updateUrl = () => setUrl(locationUrl())
  onMount(() => window.addEventListener('popstate', updateUrl))
  onCleanup(() => window.removeEventListener('popstate', updateUrl))
  const navigate = (href: string) => {
    window.history.pushState({}, '', workspacePath(href))
    window.dispatchEvent(new PopStateEvent('popstate'))
    window.scrollTo(0, 0)
  }
  const follow = (event: MouseEvent, href: string) => {
    if (event.button || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return
    event.preventDefault()
    navigate(href)
  }
  const changeQuery = (key: string, value: string) => {
    const next = new URL(url())
    next.searchParams.set(key, value)
    if (['q', 'publisher', 'tab', 'kind'].includes(key)) next.searchParams.delete('page')
    navigate(next.pathname + next.search)
  }
  const detailHref = (id: string) => workspacePath(`/apps/local/${encodeURIComponent(id.split('/').at(-1)!)}?kind=${kind()}`)
  const storeHref = (id: string, version = '', selectedSection = 'about') => workspacePath(`/apps/store/${id.split('/').map(encodeURIComponent).join('/')}?${new URLSearchParams({ kind: kind(), version, section: selectedSection })}`)
  const listHref = () => workspacePath(`/apps?${new URLSearchParams({ kind: kind(), tab: identity()?.source === 'store' ? 'store' : 'mine' })}`)
  const requestedPage = () => Math.max(1, Math.min(2147483647, Math.floor(Number(params().get('page')) || 1)))
  const [catalog, catalogActions] = createResource(
    () => !detail() && tab() === 'store' ? `${collection()}|${new URLSearchParams({ q: params().get('q') ?? '',
      publisher: params().get('publisher') ? (params().get('publisher')!.endsWith('.dhttp.net') ? params().get('publisher')! : params().get('publisher')! + '.dhttp.net') : '',
      page: String(requestedPage()), page_size: '20' })}` : false,
    abortable(async (source: string, signal) => {
      const [collection, query] = source.split('|')
      const result = await storeRead<StoreCatalog>(`/${collection}?${query}`, signal)
      const items = collection === 'apps' ? result.apps : result.libs
      if (!Array.isArray(items) || !Number.isSafeInteger(result.total) || result.total < 0 || items.some(item =>
        item.kind !== (collection === 'apps' ? 'app' : 'lib') || item.id !== `${item.publisher}/${item.name}` || !validId(item.id) || !validVersion(item.version)))
        throw new Error('Invalid store catalogue response')
      return { ...result, libs: items }
    }),
  )
  const [storeDetail, storeDetailActions] = createResource(
    () => identity()?.source === 'store' ? `${identity()!.id}|${params().get('version') ?? ''}|${kind()}` : false,
    abortable((source: string, signal) => {
      const [id, version, kind] = source.split('|')
      return readStoreDetail(id, version, signal, kind as 'app' | 'lib')
    }),
  )
  const selectedStore = () => storeDetail.error ? undefined : storeDetail()
  const current = () => identity()?.source === 'store' ? storeManifest(selectedStore()) : saved(identity()?.id ?? '')
  const publisher = (item: LibSummary | undefined) => item?.id.includes('/') ? item.id.split('/')[0].replace(/\.dhttp\.net$/, '') : t('apps.local')
  const filtered = () => installedItems().filter(item => `${item.id} ${item.title}`.toLowerCase().includes((params().get('q') ?? '').toLowerCase()))
    .sort((a, b) => a.title.localeCompare(b.title) || a.id.localeCompare(b.id))
  const page = () => Math.min(requestedPage(), Math.max(1, Math.ceil(filtered().length / 20)))
  const [context] = createResource(api.context)
  const [feedback, setFeedback] = createSignal('')
  const [dialog, setDialog] = createSignal<'publish' | 'uninstall' | StoreDetail | null>(null)
  const [target, setTarget] = createSignal('')
  const [file, setFile] = createSignal<File>()
  const [accepted, setAccepted] = createSignal(false)
  const [writing, setWriting] = createSignal<'idle' | 'saving'>('idle')
  const [actionError, setActionError] = createSignal('')
  const storeSelection = () => {
    const selected = dialog()
    return selected && typeof selected === 'object' ? selected : undefined
  }
  const openPublish = () => {
    setTarget('')
    setFile(undefined)
    setAccepted(false)
    setActionError('')
    setDialog('publish')
  }
  const openUninstall = (id: string) => { setTarget(id); setActionError(''); setDialog('uninstall') }
  const openStoreInstall = (release: StoreDetail) => {
    if (writing() !== 'idle' || release.status !== 'published') return
    setAccepted(false); setActionError(''); setDialog(release)
  }
  const prepareStoreInstall = async (release: Pick<StoreLib, 'id' | 'version'>) => {
    if (writing() !== 'idle') return
    setWriting('saving')
    setFeedback('')
    try {
      const selected = await readStoreDetail(release.id, release.version, undefined, kind())
      if (selected.kind !== kind()) return
      setWriting('idle')
      openStoreInstall(selected)
    } catch (error) { setFeedback(errorMessage(error, t('common.unexpectedError'))) }
    finally { setWriting('idle') }
  }
  const installPreview = () => storeManifest(storeSelection())
  const installId = () => storeSelection()?.id ?? target()
  const historyPage = () => Math.max(1, Math.floor(Number(params().get('history_page')) || 1))
  const [history] = createResource(
    () => detail() && identity()?.id.includes('/') && section() === 'changes' ? `${identity()!.id}|${historyPage()}|${collection()}` : false,
    abortable(async (source: string, signal) => {
      const [id, page, collection] = source.split('|')
      const result = await storeRead<StoreVersions>(`/${collection}/${id}/versions?page=${page}&page_size=20`, signal)
      if (!Array.isArray(result.versions) || result.versions.some(item => item.id !== id || !validVersion(item.version))) throw new Error('Invalid store version history')
      return result
    }),
  )
  const [updateInputs, setUpdateInputs] = createSignal<Array<Pick<StoreLib, 'publisher' | 'name' | 'version'>>>()
  const [updates] = createResource(updateInputs, abortable(async (inputs: Array<Pick<StoreLib, 'publisher' | 'name' | 'version'>>, signal) => {
    const results: Array<StoreUpdate | { id: string; status: 'failed'; message: string }> = []
    const selectedCollection = collection()
    for (let offset = 0; offset < inputs.length; offset += 100) {
      const batch = inputs.slice(offset, offset + 100)
      const expected = batch.map(item => `${item.publisher}/${item.name}`)
      try {
        const reply = await request<Partial<Record<'apps' | 'libs', StoreUpdate[]>>>(storeBase + '/updates', {
          method: 'POST', body: JSON.stringify({ [selectedCollection]: batch }), signal: AbortSignal.any([signal, AbortSignal.timeout(30_000)]),
        })
        const items = reply[selectedCollection]
        if (!Array.isArray(items) || items.length !== expected.length || new Set(items.map(item => item.id)).size !== expected.length
          || items.some(item => !expected.includes(item.id) || !['current', 'update', 'not_found', 'unavailable'].includes(item.status)
            || item.status === 'update' && (!validVersion(item.version) || !newerVersion(item.version, batch[expected.indexOf(item.id)].version))))
          throw new Error(t('apps.invalidUpdateResponse'))
        results.push(...items)
      } catch (error) {
        if (signal.aborted) throw error
        results.push(...expected.map(id => ({ id, status: 'failed' as const, message: errorMessage(error, t('common.unexpectedError')) })))
      }
    }
    setFeedback(results.some(item => item.status === 'failed') ? t('apps.checkFailed') : results.some(item => item.status === 'update') ? '' : t('apps.current'))
    return results
  }))
  createEffect(on(installedItems, () => setUpdateInputs(undefined), { defer: true }))
  createEffect(on(kind, () => { setFeedback(''); setDialog(null); setUpdateInputs(undefined) }, { defer: true }))
  const checkUpdates = () => {
    setFeedback('')
    setUpdateInputs(installedItems().filter(item => validId(item.id) && item.id.includes('/') && validVersion(item.version)).map(item => {
      const [publisher, name] = item.id.split('/')
      return { publisher, name, version: item.version }
    }))
    const next = new URL(url()); next.searchParams.set('tab', 'mine'); next.searchParams.set('updates', '1'); navigate(next.pathname + next.search)
  }
  const saveComponent = async (event: SubmitEvent) => {
    event.preventDefault()
    if (writing() !== 'idle') return
    const input = file(), release = storeSelection(), mode = dialog()
    if (mode === 'publish' && (!input || input.size > 64 * 1024 * 1024)) { setActionError(t('apps.fileLimit')); return }
    if (mode !== 'publish' && mode !== 'uninstall' && !release) return
    if (mode !== 'uninstall' && !accepted()) return
    let id = release?.id ?? target()
    if (mode === 'publish' && id.includes('/') && !id.split('/')[0].endsWith('.dhttp.net')) id = id.replace('/', '.dhttp.net/')
    if (!validId(id)) { setActionError(t('apps.invalidId')); return }
    const version = String(new FormData(event.currentTarget as HTMLFormElement).get('version') ?? '')
    if (mode === 'publish' && (!id.includes('/') || !validVersion(version))) { setActionError(t('apps.publishInvalid')); return }
    if (mode === 'publish' && (context.error || id.split('/')[0] !== context()?.owner_name)) { setActionError(t('apps.publishIdentity')); return }
    setWriting('saving'); setActionError('')
    try {
      if (mode === 'publish') {
        const result = await request<StoreDetail>(`${storeBase}/${collection()}/${id}/versions/${version}`, {
          method: 'PUT', headers: { 'content-type': 'application/webc' }, body: input!, signal: AbortSignal.timeout(180_000),
        })
        if (result.id !== id || result.version !== version || !['published', 'yanked'].includes(result.status)) throw new Error(t('apps.publishUnknown'))
        setFeedback(t(result.status === 'published' ? 'apps.publishSuccess' : 'apps.publishYanked'))
        setDialog(null); navigate(storeHref(id, version)); void catalogActions.refetch()
      } else {
        const result = await request<LibSummary>(`/std/sys/${release ? release.kind : kind()}?name=${encodeURIComponent(id.split('/').at(-1)!)}`, release
          ? { method: 'PUT', body: JSON.stringify({ publisher: release.publisher, version: release.version }) }
          : { method: 'DELETE', headers: { 'expected-package-id': id } })
        if (release && (result.id !== id || result.version !== release.version || result.kind !== release.kind)) throw new Error(t('apps.verifyHint'))
        setFeedback(t(mode === 'uninstall' ? 'apps.uninstalled' : 'apps.active')); setDialog(null); await installedActions.refetch()
      }
    } catch (error) { setActionError(errorMessage(error, t('common.unexpectedError'))) }
    finally { setWriting('idle') }
  }
  const dependencies = (item: LibSummary | undefined) => <Show when={item?.dependencies?.length} fallback={<p class="muted">{t('apps.noDependencies')}</p>}>
    <ul class="app-operations"><For each={item?.dependencies}>{dependency => <li>
      <a class="text-link" href={workspacePath(`/apps/${identity()?.source === 'store' ? `store/${encodeURIComponent(dependency.publisher)}` : 'local'}/${encodeURIComponent(dependency.name)}?kind=lib&version=${encodeURIComponent(dependency.version)}`)}>
        <code>{dependency.publisher.replace(/\.dhttp\.net$/, '')}/{dependency.name}</code>
      </a><p>v{dependency.version}</p>
    </li>}</For></ul>
  </Show>
  const interfaces = (item: LibSummary | undefined) => <>
    {dependencies(item)}
    <Show when={identity()?.source === 'store' && item?.kind !== 'app'}><ul class="app-operations">
      <For each={operations(item).filter(op => !pageResource(op))}>{op => <li><code>{op.method} {op.path}</code><p>{op.description}</p></li>}</For>
    </ul></Show>
    <Show when={identity()?.source === 'local' && item?.kind !== 'app'}>
      <a class="text-link" href={workspacePath('/settings/capabilities')}>{t('nav.capabilities')}</a>
    </Show>
  </>
  const actions = (item: LibSummary, details = false) => <div class="button-group app-actions">
    <Show when={entry(item)} keyed>{href => <a class="button button-primary" href={href} target="_blank" rel="noopener noreferrer">{t('apps.open')} ↗</a>}</Show>
    <Show when={details}><a class="button button-secondary" href={detailHref(item.id)} onClick={event => follow(event, detailHref(item.id))}>{t('apps.details')}</a></Show>
    <Show when={item.id.includes('/') && validId(item.id)}><button class="button button-secondary" disabled={writing() !== 'idle'} onClick={() => void prepareStoreInstall({ id: item.id, version: '' })}>{t('apps.update')}</button></Show>
    <button class="button button-danger-quiet" onClick={() => openUninstall(item.id)}>{t('apps.uninstall')}</button>
  </div>
  const storeActions = (release: StoreDetail) => <div class="button-group app-actions">
    <Show when={saved(release.id) && entry(saved(release.id))} keyed>{href => <a class="button button-secondary" href={href} target="_blank" rel="noopener noreferrer">{t('apps.open')} ↗</a>}</Show>
    <Show when={release.status === 'published'} fallback={<p class="app-notice">{t('apps.yanked')}: {release.status_reason}</p>}>
      <button class="button button-primary" disabled={writing() !== 'idle' || installed.loading || !!installed.error} onClick={() => openStoreInstall(release)}>{t('apps.installVersion', { version: release.version })}</button>
    </Show>
  </div>
  return <div class="apps-page">
    <Show when={feedback()}><div class="app-notice app-notice-dismissible" role="status"><span>{feedback()}</span>
      <button class="icon-button" aria-label={t('common.dismissMessage')} onClick={() => setFeedback('')}>×</button></div></Show>
    <Show when={installed.error}><ErrorState message={errorMessage(installed.error, t('common.unexpectedError'))} onRetry={() => void installedActions.refetch()} /></Show>
    <Show when={detail()} fallback={<>
      <header class="page-header"><h1>{t('nav.apps')}</h1><Show when={tab() === 'store'}><button class="button button-primary" onClick={openPublish}>{t('apps.publish')}</button></Show></header>
      <nav class="section-nav app-tabs"><For each={['mine', 'store'] as const}>{item => <button aria-current={tab() === item ? 'page' : undefined} onClick={() => changeQuery('tab', item)}>{t(`apps.${item}`)}</button>}</For></nav>
      <form class="app-search" onSubmit={event => {
        event.preventDefault(); const data = new FormData(event.currentTarget), next = new URL(url())
        for (const key of ['q', 'publisher']) next.searchParams.set(key, String(data.get(key) ?? ''))
        next.searchParams.delete('page'); navigate(next.pathname + next.search)
      }}>
        <select aria-label={t('apps.artifactKind')} value={kind()} onChange={event => changeQuery('kind', event.currentTarget.value)}><option value="app">{t('apps.frontend')}</option><option value="lib">{t('apps.backend')}</option></select>
        <input name="q" type="search" aria-label={t('apps.search')} placeholder={t('apps.search')} value={params().get('q') ?? ''} />
        <Show when={tab() === 'store'}><input name="publisher" aria-label={t('apps.publisher')} placeholder={t('apps.publisher')} value={params().get('publisher') ?? ''} /></Show>
        <button class="button button-secondary" type="submit">{t('apps.search')}</button>
        <button class="button button-secondary" type="button" disabled={installed.loading || !!installed.error || updates.loading} onClick={checkUpdates}>{t('apps.check')}</button>
        <button class="button button-secondary" type="button" disabled={installed.loading} onClick={() => void installedActions.refetch()}>{t('common.refresh')}</button>
      </form>
      <Show when={tab() === 'mine'}>
        <Show when={installed.loading && !installedItems().length}><LoadingState label={t('apps.loading')} /></Show>
        <Show when={!installed.loading && !installed.error && !installedItems().length}><EmptyState title={t('apps.noApps')} /></Show>
        <Show when={params().get('updates') === '1'}>
          <Show when={updates.loading}><LoadingState /></Show>
          <ul class="app-update-list" aria-label={t('apps.updateResults')}><For each={updates()}>{item => <li>
            <a class="text-link" href={storeHref(item.id, item.status === 'update' ? item.version : '')}>{saved(item.id)?.title ?? item.id}</a>
            {item.status === 'update' && <button class="button button-primary" disabled={writing() !== 'idle'} onClick={() => void prepareStoreInstall(item)}>{t('apps.updateAvailable', { version: item.version })}</button>}
            {item.status === 'failed' && <p class="field-error">{item.message}</p>}
          </li>}</For></ul>
        </Show>
        <section class="app-grid" aria-label={t('apps.mine')}><For each={filtered().slice((page() - 1) * 20, page() * 20)}>{item => <article class="app-card">
          <a class="app-card-title" href={detailHref(item.id)} onClick={event => follow(event, detailHref(item.id))}><span class="app-icon">{item.title.slice(0, 1).toUpperCase()}</span><h2>{item.title}</h2></a>
          <p class="app-meta">{publisher(item)}</p><p class="app-description">{item.description ?? t('apps.notProvided')}</p><p class="app-meta">v{item.version}</p>{actions(item, true)}
        </article>}</For></section>
        <Pagination page={page()} pageSize={20} total={filtered().length} onPage={next => changeQuery('page', String(next))} />
      </Show>
      <Show when={tab() === 'store'}>
        <Show when={catalog.loading}><LoadingState /></Show><Show when={catalog.error}><ErrorState message={errorMessage(catalog.error, t('common.unexpectedError'))} onRetry={() => void catalogActions.refetch()} /></Show>
        <Show when={!catalog.loading && !catalog.error && catalog()?.total === 0}><EmptyState title={t('apps.storeEmpty')} /></Show>
        <section class="app-grid"><For each={catalog()?.libs}>{release => <article class="app-card">
          <a class="app-card-title" href={storeHref(release.id, release.version)} onClick={event => follow(event, storeHref(release.id, release.version))}><h2>{release.title}</h2></a>
          <p class="app-meta">{release.publisher.replace(/\.dhttp\.net$/, '')}</p><p class="app-description">{release.description}</p><p class="app-meta">v{release.version}</p>
          <button class="button button-primary" disabled={writing() !== 'idle' || release.status !== 'published' || !!installed.error} onClick={() => void prepareStoreInstall(release)}>{t('apps.installVersion', { version: release.version })}</button>
        </article>}</For></section>
        <Pagination page={requestedPage()} pageSize={20} total={catalog()?.total ?? 0} onPage={next => changeQuery('page', String(next))} />
      </Show>
    </>}>
      <a class="text-link app-back" href={listHref()} onClick={event => follow(event, listHref())}>← {t('apps.back')}</a>
      <Show when={identity()} fallback={<ErrorState message={t('apps.invalidId')} />}>
        <Show when={storeDetail.loading || installed.loading}><LoadingState /></Show>
        <Show when={storeDetail.error && identity()?.source === 'store'}><ErrorState message={errorMessage(storeDetail.error, t('common.unexpectedError'))} onRetry={() => void storeDetailActions.refetch()} /></Show>
        <Show when={current()} fallback={<Show when={!installed.loading && !storeDetail.loading && !storeDetail.error && !installed.error}><EmptyState title={t('apps.missing')} /></Show>} keyed>{item => <>
          <header class="app-detail-header"><div class="app-identity"><span class="app-icon">{item.title.slice(0, 1)}</span><div><h1>{item.title}</h1><p class="app-meta">{publisher(item)} · v{item.version}</p></div></div>
            <Show when={selectedStore()} fallback={actions(item)} keyed>{release => storeActions(release)}</Show></header>
          <nav class="section-nav app-tabs" aria-label={t('apps.details')}><For each={(['about', 'changes', 'interfaces', 'access', 'trust'] as const).filter(name => name !== 'trust' || identity()?.source === 'store')}>{name => <button aria-current={section() === name ? 'page' : undefined} onClick={() => changeQuery('section', name)}>{t(name === 'interfaces' && kind() === 'app' ? 'apps.dependencies' : `apps.${name}`)}</button>}</For></nav>
          <Show when={section() === 'about'}><div class="app-about"><section class="app-panel"><h2>{t('apps.about')}</h2><p class="app-prose">{item.description ?? t('apps.notProvided')}</p></section>
            <aside class="app-panel app-facts"><h2>{t('apps.fullId')}</h2><code>{item.id}</code><dl>
              <dt>{t('apps.declared')}</dt><dd>{publisher(item)}</dd><dt>{t('apps.savedVersion')}</dt><dd>{saved(item.id)?.version ?? '—'}</dd>
              <Show when={repositoryUrl(item.repository)} keyed>{href => <><dt>{t('apps.repository')}</dt><dd><a class="text-link" href={href} target="_blank" rel="noopener noreferrer">{href}</a></dd></>}</Show>
              <dt>{t('apps.license')}</dt><dd>{item.license ?? t('apps.notProvided')}</dd></dl></aside></div></Show>
          <Show when={section() === 'interfaces'}><section class="app-panel"><h2>{t(kind() === 'app' ? 'apps.dependencies' : 'apps.interfaces')}</h2>{interfaces(item)}</section></Show>
          <Show when={section() === 'access'}><section class="app-panel"><a class="text-link" href={workspacePath('/settings/capabilities')}>{t('nav.capabilities')}</a> · <a class="text-link" href={workspacePath('/settings/access')}>{t('nav.policies')}</a></section></Show>
          <Show when={section() === 'trust'}><section class="app-panel"><h2>{t('apps.trust')}</h2><Show when={selectedStore()} keyed>{release => <>
            <dl class="app-facts"><dt>{t('apps.publisherOwner')}</dt><dd><code>{release.publisher_owner_hash}</code></dd><dt>{t('apps.packageDigest')}</dt><dd><code>{release.package_sha256}</code></dd><dt>{t('apps.packageSize')}</dt><dd>{release.size.toLocaleString()} B</dd></dl>
            <dl><For each={Object.entries(release.host ?? {})}>{([name, purpose]) => <><dt>{name}</dt><dd>{purpose}</dd></>}</For></dl>
          </>}</Show></section></Show>
          <Show when={section() === 'changes'}><section class="app-panel"><h2>{t('apps.changes')}</h2><p class="app-prose">{item.changelog ?? t('apps.notProvided')}</p>
            <Show when={history.loading}><LoadingState /></Show><Show when={history.error}><ErrorState message={errorMessage(history.error, t('common.unexpectedError'))} /></Show>
            <ul class="app-operations"><For each={history()?.versions}>{release => <li><a class="text-link" href={storeHref(release.id, release.version, 'changes')} onClick={event => follow(event, storeHref(release.id, release.version, 'changes'))}>v{release.version}</a><p>{release.changelog}</p><p>{release.status_reason}</p></li>}</For></ul>
            <button class="button button-secondary" disabled={historyPage() === 1} onClick={() => changeQuery('history_page', String(historyPage() - 1))}>{t('apps.historyPrevious')}</button>
            <button class="button button-secondary" disabled={(history()?.versions.length ?? 0) < 20} onClick={() => changeQuery('history_page', String(historyPage() + 1))}>{t('apps.historyNext')}</button>
          </section></Show>
        </>}</Show>
      </Show>
    </Show>
    <Dialog open={dialog() !== null} wide title={t(dialog() === 'publish' ? 'apps.publish' : dialog() === 'uninstall' ? 'apps.confirmUninstall' : 'apps.storeConfirm')} onClose={() => writing() === 'idle' && setDialog(null)}>
      <form class="form-grid app-install" onSubmit={saveComponent}>
        <p class="field-wide">{t('apps.target')}: <code>{context()?.owner_name ?? t('app.connecting')}</code></p>
        <Show when={dialog() === 'uninstall'} fallback={<>
          <Show when={dialog() === 'publish'}>
            <label class="field field-wide"><span>{t('apps.fullId')}</span><input value={target()} required pattern="[a-z0-9.-]+/[a-z][a-z0-9-]{0,62}" disabled={writing() !== 'idle'} onInput={event => setTarget(event.currentTarget.value)} /></label>
            <label class="field field-wide"><span>{t('apps.releaseVersion')}</span><input name="version" required pattern="[0-9]+\.[0-9]+\.[0-9]+" disabled={writing() !== 'idle'} /></label>
            <label class="field field-wide"><span>{t('apps.component')}</span><input type="file" accept=".webc,application/webc" required disabled={writing() !== 'idle'} onChange={event => { setFile(event.currentTarget.files?.[0]); setAccepted(false); setActionError('') }} /></label>
          </Show>
          <Show when={storeSelection()} keyed>{release => <section class="field-wide app-panel"><h3>{release.title} · v{release.version}</h3><code>{release.id}</code><p>{release.changelog}</p><p>{t('apps.savedVersion')}: {saved(release.id)?.version ?? '—'}</p>
            <details class="app-preview-interfaces"><summary>{t(release.kind === 'app' ? 'apps.dependencies' : 'apps.interfaces')}</summary>{interfaces(installPreview())}</details>
            <dl><dt>{t('apps.packageDigest')}</dt><dd><code>{release.package_sha256}</code></dd><dt>{t('apps.publisherOwner')}</dt><dd><code>{release.publisher_owner_hash}</code></dd></dl></section>}</Show>
          <Show when={storeSelection()}><p class="field-wide">{t('apps.storeInstallHint')}</p></Show>
          <label class="app-checkbox field-wide"><input type="checkbox" checked={accepted()} onChange={event => setAccepted(event.currentTarget.checked)} disabled={writing() !== 'idle'} />{t(dialog() === 'publish' ? 'apps.publishAccept' : 'apps.accept')}</label>
        </>}><h3 class="field-wide">{saved(target())?.title ?? target()}</h3><code class="field-wide">{target()}</code><p class="field-wide">{t('apps.uninstallHint')}</p></Show>
        <Show when={actionError()}><p class="field-error field-wide" role="alert">{actionError()}</p></Show>
        <div class="dialog-actions field-wide"><button class="button button-secondary" type="button" disabled={writing() !== 'idle'} onClick={() => setDialog(null)}>{t('common.cancel')}</button>
          <button class={`button ${dialog() === 'uninstall' ? 'button-danger' : 'button-primary'}`} type="submit" disabled={writing() !== 'idle' || dialog() !== 'uninstall' && (!accepted() || !storeSelection() && !file())}>
            {writing() !== 'idle' ? t('common.saving') : t(dialog() === 'publish' ? 'apps.publishConfirm' : dialog() === 'uninstall' ? 'apps.confirmUninstall' : saved(installId()) ? 'apps.confirmUpdate' : 'apps.confirmInstall')}
          </button></div>
      </form>
    </Dialog>
  </div>
}
