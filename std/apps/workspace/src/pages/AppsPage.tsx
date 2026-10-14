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

import { api, ApiError, request } from '../api/client'
import type { Effect, LibSummary, RuleRow, StoreLib, StoreCatalog, StoreDetail, StoreVersions, StoreUpdate } from '../api/types'
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
        endpoints.push({ method: method.toUpperCase(), path: `/${release.kind === 'app' ? 'app' : 'api'}/${release.id.split('/').at(-1)}${path}`, description: op.summary ?? op.description ?? null })
    }
  }
  return { kind: release.kind, dependencies: release.dependencies, id: release.id, title: release.title, version: release.version,
    description: release.description, repository: release.repository, changelog: release.changelog ?? undefined, host: release.host ?? undefined,
    entry: release.kind === 'app' ? '/' : undefined, endpoints, openapi: release.openapi }
}

function operations(lib: LibSummary | undefined) {
  return (lib?.endpoints ?? [])
    .map((endpoint) => {
      const prefix = `/${lib!.kind === 'app' ? 'app' : 'api'}/${lib!.id.split('/').at(-1)}`
      const path = endpoint.path.startsWith(prefix + '/')
        ? endpoint.path.slice(prefix.length)
        : endpoint.path
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
  const target = `/std/${lib.id.split('/').at(-1)}${path}`
  return lib.kind === 'app' ? target : undefined
}

// Compare actual manifests without creating a second version or permission model.
function manifestChanges(
  before: LibSummary | undefined,
  after: LibSummary | undefined,
) {
  const manifest = (lib: LibSummary | undefined) => {
    const items: Record<string, string> = {}
    for (const op of lib?.kind === 'app' ? [] : operations(lib)) {
      const path = op.path.replace(`/${lib?.kind === 'app' ? 'app' : 'api'}/${lib?.id?.split('/').at(-1)}`, '')
      items[`${op.method} ${path}`] = JSON.stringify(
        op.operation ?? op.description,
      )
    }
    for (const dependency of lib?.dependencies ?? [])
      items[`dependency: ${dependency.publisher}/${dependency.name}`] = dependency.version
    const extension = lib
    for (const [name, purpose] of Object.entries(extension?.host ?? {}))
      items[`host: ${name}`] = purpose ?? ''
    for (const [name, group] of Object.entries(
      lib?.openapi?.['x-pishoo']?.['access-groups'] ?? {},
    ))
      items[`group: ${name}`] = JSON.stringify(group)
    if (extension?.entry) items.entry = extension.entry
    return items
  }
  const previous = manifest(before),
    next = manifest(after)
  return [...new Set([...Object.keys(previous), ...Object.keys(next)])]
    .sort()
    .filter((key) => previous[key] !== next[key])
    .map((key) => ({ key, before: previous[key], after: next[key] }))
}

export default function AppsPage() {
  const { t } = useI18n()
  const locationUrl = () => new URL(window.location.hash.slice(1) || '/apps', window.location.origin)
  const [url, setUrl] = createSignal(locationUrl())
  const params = () => url().searchParams
  const kind = () => params().get('kind') === 'lib' ? 'lib' as const : 'app' as const
  const collection = () => kind() === 'app' ? 'apps' : 'libs'
  const tab = () => params().get('tab') === 'store' ? 'store' : 'mine'
  const section = () =>
    ['changes', 'interfaces', 'access', 'trust'].includes(
      params().get('section') ?? '',
    )
      ? params().get('section')!
      : 'about'
  const detail = () =>
    url().pathname !== '/apps' &&
    url().pathname !== '/apps/'
  const [loaded, loadedActions] = createResource(
    kind,
    abortable((kind: 'app' | 'lib', signal) => request<LibSummary[]>(`/std/sys/package/loaded-${kind === 'app' ? 'apps' : 'libs'}`, { signal })),
  )
  const [disk, diskActions] = createResource(
    kind,
    abortable((kind: 'app' | 'lib', signal) =>
      request<Array<LibSummary | { id: string; error: string }>>(
        `/std/sys/package/${kind === 'app' ? 'apps' : 'libs'}`,
        { signal },
      ),
    ),
  )
  const loadedItems = () => (loaded.error ? undefined : loaded.latest?.filter(lib => (lib.kind ?? 'lib') === kind()))
  const diskItems = () => (disk.error ? undefined : disk.latest?.filter(lib => 'error' in lib || (lib.kind ?? 'lib') === kind()))
  const identity = () => {
    const match = /^\/apps\/(local|store)\/(.+?)\/?$/.exec(
      url().pathname,
    )
    if (!match) return null
    try {
      const segments = match[2].split('/').map(decodeURIComponent)
      if (segments.some((s) => s.includes('/')) || !validId(segments.join('/')))
        return null
      if (match[1] === 'store') {
        if (segments.length !== 2) return null
        return { source: match[1], id: segments.join('/') }
      }
      if (segments.length !== 1) return null
      const name = segments[0]
      const matches = [...new Set([
        ...(loadedItems() ?? []).map(lib => lib.id),
        ...(diskItems() ?? []).map(lib => lib.id),
      ])].filter(id => id.split('/').at(-1) === name)
      if (matches.length > 1) return null
      return { source: match[1], id: matches[0] ?? name }
    } catch {
      return null
    }
  }
  createEffect(
    on(
      () => `${section()}/${params().get('view')}/${current()?.id}`,
      () => {
        queueMicrotask(() => {
          for (const selected of document.querySelectorAll<HTMLElement>(
            '.apps-page .app-tabs [aria-current=page]',
          )) {
            const nav = selected.parentElement!
            const offset =
              selected.getBoundingClientRect().left -
              nav.getBoundingClientRect().left
            if (offset < 0) nav.scrollLeft += offset
            else if (offset + selected.offsetWidth > nav.clientWidth)
              nav.scrollLeft += offset + selected.offsetWidth - nav.clientWidth
          }
        })
      },
    ),
  )
  const updateUrl = () => setUrl(locationUrl())
  onMount(() => { window.addEventListener('popstate', updateUrl); window.addEventListener('hashchange', updateUrl) })
  onCleanup(() => { window.removeEventListener('popstate', updateUrl); window.removeEventListener('hashchange', updateUrl) })
  const navigate = (href: string, scroll = 0) => {
    window.history.replaceState(
      { ...window.history.state, appsScroll: window.scrollY },
      '',
      window.location.href,
    )
    window.history.pushState({ appsScroll: scroll }, '', workspacePath(href))
    window.dispatchEvent(new PopStateEvent('popstate'))
    window.scrollTo(0, scroll)
  }
  const follow = (event: MouseEvent, href: string, scroll = 0) => {
    if (
      event.button ||
      event.metaKey ||
      event.ctrlKey ||
      event.shiftKey ||
      event.altKey
    )
      return
    event.preventDefault()
    navigate(href, scroll)
  }
  const changeQuery = (key: string, value: string) => {
    const next = new URL(url())
    next.searchParams.set(key, value)
    if (key === 'q' || key === 'publisher' || key === 'tab' || key === 'kind')
      next.searchParams.delete('page')
    navigate(next.pathname + next.search)
  }
  const listHref = () => {
    const query = new URLSearchParams(params())
    for (const key of ['section', 'grantee', 'view', 'selected', 'scroll'])
      query.delete(key)
    return workspacePath(`/apps${query.size ? '?' + query : ''}`)
  }
  const detailHref = (id: string, selectedSection = 'about') => {
    const query = new URLSearchParams(params())
    query.set('section', selectedSection)
    query.set('scroll', String(window.scrollY))
    return workspacePath(`/apps/local/${encodeURIComponent(id.split('/').at(-1)!)}?${query}`)
  }
  const storeHref = (id: string, version = '', selectedSection = 'about') => {
    const query = new URLSearchParams(params())
    query.set('section', selectedSection)
    query.set('scroll', String(window.scrollY))
    query.delete('history_page')
    if (version) query.set('version', version)
    else query.delete('version')
    return workspacePath(`/apps/store/${id.split('/').map(encodeURIComponent).join('/')}?${query}`)
  }
  const requestedPage = () => Math.max(1, Math.min(2147483647, Math.floor(Number(params().get('page')) || 1)))
  const [catalog, catalogActions] = createResource(
    () => !detail() && tab() === 'store' ? new URLSearchParams({
      kind: kind(), q: params().get('q') ?? '', publisher: params().get('publisher') ?? '', page: String(requestedPage()), page_size: '20',
    }).toString() : false,
    abortable(async (query: string, signal) => {
      const selected = new URLSearchParams(query)
      const kind = selected.get('kind') === 'app' ? 'app' : 'lib'
      selected.delete('kind')
      const result = await storeRead<StoreCatalog>(`/${kind === 'app' ? 'apps' : 'libs'}?${selected}`, signal)
      const items = kind === 'app' ? result.apps : result.libs
      if (!Array.isArray(items) || !Number.isSafeInteger(result.total) || result.total < 0
        || items.some(lib => (lib.kind ?? 'lib') !== kind || !validId(lib.id) || lib.id !== `${lib.publisher}/${lib.name}` || !validVersion(lib.version)))
        throw new Error('Invalid store catalogue response')
      return { ...result, libs: items }
    }),
  )
  const [storeDetail, storeDetailActions] = createResource(
    () => detail() && identity()?.source === 'store'
      ? `${identity()!.id}|${params().get('version') ?? ''}|${kind()}` : false,
    abortable((source: string, signal) => {
      const [id, version, kind] = source.split('|')
      return readStoreDetail(id, version, signal, kind as 'app' | 'lib')
    }),
  )
  const [context] = createResource(api.context)
  const refresh = () => {
    void loadedActions.refetch()
    void diskActions.refetch()
  }
  const running = (id: string) => loadedItems()?.find((lib) => lib.id === id)
  const saved = (id: string) => diskItems()?.find((lib) => lib.id === id)
  const metadata = (id: string) => {
    const installed = saved(id)
    return (
      running(id) ??
      (installed && !('error' in installed) ? installed : undefined)
    )
  }
  const selectedStore = () => {
    if (storeDetail.error) return undefined
    const result = storeDetail()
    return result && (result.kind ?? 'lib') === kind() && result.id === identity()?.id && (!params().get('version') || result.version === params().get('version')) ? result : undefined
  }
  const current = () => identity()?.source === 'store' ? storeManifest(selectedStore()) : identity() ? metadata(identity()!.id) : undefined
  const currentRunning = () =>
    identity()?.source === 'local' ? running(identity()!.id) : undefined
  const ids = () =>
    [
      ...new Set([
        ...(loadedItems() ?? []).map((lib) => lib.id),
        ...(diskItems() ?? []).map((lib) => lib.id),
      ]),
    ].filter(validId)
  const filtered = () =>
    ids()
      .filter((id) => {
        const lib = metadata(id)
        return `${id} ${lib?.title ?? ''} ${lib?.id.includes('/') ? lib.id.split('/')[0] : ''}`
          .toLowerCase()
          .includes((params().get('q') ?? '').toLowerCase())
      })
      .sort(
        (a, b) =>
          (metadata(a)?.title ?? a).localeCompare(metadata(b)?.title ?? b) ||
          a.localeCompare(b),
      )
  const page = () =>
    Math.min(
      Math.max(1, Number(params().get('page')) || 1),
      Math.max(1, Math.ceil(filtered().length / 20)),
    )
  const publisher = (lib: LibSummary | undefined) =>
    lib?.id.includes('/') ? lib.id.split('/')[0] : t('apps.local')
  const stateText = (id: string) => {
    const installed = saved(id)
    if (disk.error) return t('apps.unknownDisk')
    if (installed && 'error' in installed)
      return `${t('apps.invalidFile')}: ${installed.error}`
    if (loaded.error) return t('apps.unknownRuntime')
    if (running(id) && !installed && disk.state === 'ready')
      return t('apps.removed')
    if (
      installed &&
      (!running(id) || installed.version !== running(id)?.version)
    )
      return `${t('apps.pending')} · v${installed.version}`
    return ''
  }
  const [feedback, setFeedback] = createSignal('')
  const [dismissedStateNotice, setDismissedStateNotice] = createSignal('')
  const stateNoticeKey = () => `${identity()?.id}:${identity() ? stateText(identity()!.id) : ''}`
  const detailNotice = () => feedback() || (dismissedStateNotice() !== stateNoticeKey() && identity() ? stateText(identity()!.id) : '')
  const [dialog, setDialog] = createSignal<'file' | 'publish' | 'uninstall' | StoreDetail | null>(null)
  const storeSelection = () => {
    const value = dialog()
    return value && typeof value === 'object' ? value : undefined
  }
  const [target, setTarget] = createSignal('')
  const [file, setFile] = createSignal<File>()
  const [inspected, setInspected] = createSignal<LibSummary>()
  const [accepted, setAccepted] = createSignal(false)
  // One request phase prevents duplicate submissions; no persisted install state.
  const [writing, setWriting] = createSignal<'idle' | 'checking' | 'saving'>(
    'idle',
  )
  const [actionError, setActionError] = createSignal('')
  const openFile = (id = '', mode: 'file' | 'publish' = 'file') => {
    setTarget(id)
    setFile(undefined)
    setInspected(undefined)
    setAccepted(false)
    setActionError('')
    setDialog(mode)
  }
  const openUninstall = (id: string) => {
    setTarget(id)
    setActionError('')
    setDialog('uninstall')
  }
  const openStoreInstall = (release: StoreDetail) => {
    if (writing() !== 'idle' || release.status !== 'published') return
    setAccepted(false)
    setActionError('')
    setDialog(release)
  }
  const prepareStoreInstall = async (release: Pick<StoreLib, 'id' | 'version'>) => {
    if (writing() !== 'idle') return
    setWriting('checking')
    setFeedback('')
    try {
      const detail = await readStoreDetail(release.id, release.version, undefined, kind())
      if (detail.status !== 'published') throw new Error(t('apps.yanked'))
      setWriting('idle')
      openStoreInstall(detail)
    } catch (error) {
      setFeedback(errorMessage(error, t('common.unexpectedError')))
    } finally { setWriting('idle') }
  }
  const installPreview = () => storeManifest(storeSelection()) ?? inspected()
  const installId = () => storeSelection()?.id ?? target()

  const historyPage = () => Math.max(1, Math.floor(Number(params().get('history_page')) || 1))
  const [history, historyActions] = createResource(
    () => detail() && identity()?.id.includes('/') && section() === 'changes'
      ? `${identity()!.id}|${historyPage()}|${kind()}` : false,
    abortable(async (source: string, signal) => {
      const [id, page, kind] = source.split('|')
      const result = await storeRead<StoreVersions>(`/${kind === 'app' ? 'apps' : 'libs'}/${id}/versions?page=${page}&page_size=20`, signal)
      if (!Array.isArray(result.versions) || result.versions.some(release => release.id !== id || !validVersion(release.version)))
        throw new Error('Invalid store version history')
      return result
    }),
  )
  const eligible = () => (diskItems() ?? []).filter((lib): lib is LibSummary =>
    !('error' in lib) && validId(lib.id) && lib.id.includes('/') && validVersion(lib.version),
  )
  const [updateInputs, setUpdateInputs] = createSignal<Array<Pick<StoreLib, 'publisher' | 'name' | 'version'>>>()
  const [updates] = createResource(
    updateInputs,
    abortable(async (inputs: Array<Pick<StoreLib, 'publisher' | 'name' | 'version'>>, signal) => {
      const selectedCollection = collection()
      const results: Array<StoreUpdate | { id: string; status: 'failed'; message: string }> = []
      for (let offset = 0; offset < inputs.length; offset += 100) {
        const batch = inputs.slice(offset, offset + 100)
        const expected = batch.map(lib => `${lib.publisher}/${lib.name}`)
        try {
          const reply = await request<Partial<Record<'apps' | 'libs', StoreUpdate[]>>>(storeBase + '/updates', {
            method: 'POST', body: JSON.stringify({ [selectedCollection]: batch }),
            signal: AbortSignal.any([signal, AbortSignal.timeout(30_000)]),
          })
          const response = { libs: reply[selectedCollection] }
          if (!Array.isArray(response.libs) || response.libs.length !== expected.length
            || new Set(response.libs.map(lib => lib.id)).size !== expected.length
            || response.libs.some(lib => !expected.includes(lib.id) || !['current', 'update', 'not_found', 'unavailable'].includes(lib.status)
              || lib.status === 'update' && (!validVersion(lib.version)
                || !newerVersion(lib.version, batch[expected.indexOf(lib.id)].version))))
            throw new Error(t('apps.invalidUpdateResponse'))
          results.push(...response.libs)
        } catch (error) {
          if (signal.aborted) throw error
          results.push(...expected.map(id => ({ id, status: 'failed' as const, message: errorMessage(error, t('common.unexpectedError')) })))
        }
      }
      if (results.some(result => result.status === 'failed')) setFeedback(t('apps.checkFailed'))
      else if (!results.some(result => result.status === 'update')) setFeedback(t('apps.current'))
      return results
    }),
  )
  createEffect(on(diskItems, () => setUpdateInputs(undefined), { defer: true }))
  createEffect(on(kind, () => {
    setFeedback('')
    setUpdateInputs(undefined)
    setDialog(null)
    setInspected(undefined)
    setAccepted(false)
  }, { defer: true }))
  const checkUpdates = () => {
    if (disk.loading || disk.error || updates.loading) return
    setFeedback('')
    setUpdateInputs(eligible().map(lib => {
      const [publisher, name] = lib.id.split('/')
      return { publisher, name, version: lib.version }
    }))
    const next = new URL(url())
    next.searchParams.set('tab', 'mine')
    next.searchParams.set('updates', '1')
    next.searchParams.delete('page')
    navigate(next.pathname + next.search)
  }
  const inspectFile = async () => {
    const input = file()
    if (!input || input.size > 64 * 1024 * 1024) {
      setActionError(t('apps.fileLimit'))
      return
    }
    setWriting('checking')
    setActionError('')
    setInspected(undefined)
    setAccepted(false)
    try {
      const result = await request<LibSummary>(`/std/sys/package/${kind()}-check`, {
        method: 'POST',
        headers: { 'content-type': 'application/webc' },
        body: input,
      })
      if ((result.kind ?? 'lib') !== kind()) throw new Error(t('apps.kindMismatch'))
      if (
        result.openapi?.['x-pishoo'] &&
        'templates' in result.openapi['x-pishoo']
      )
        throw new Error(t('apps.templatesUnsupported'))
      if (dialog() === 'publish') {
        const id = result.id
        const publisher = id.split('/')[0]
        if (!validId(id) || !validVersion(result.version))
          throw new Error(t('apps.publishInvalid'))
        if (context.error || !context()?.owner_name || publisher !== context()?.owner_name)
          throw new Error(t('apps.publishIdentity'))
        setTarget(id)
      }
      setInspected(result)
    } catch (error) {
      setActionError(errorMessage(error, t('common.unexpectedError')))
    } finally {
      setWriting('idle')
    }
  }
  const saveComponent = async (event: SubmitEvent) => {
    event.preventDefault()
    if (writing() !== 'idle') return
    if (dialog() === 'publish') {
      const manifest = inspected(), input = file()
      if (!manifest || !input || !accepted()) return
      const publisher = manifest.id.split('/')[0]
      if (context.error || !context()?.owner_name || publisher !== context()?.owner_name) {
        setActionError(t('apps.publishIdentity'))
        return
      }
      const id = manifest.id, version = manifest.version
      if (!validId(id) || !validVersion(version)) return
      setWriting('saving')
      setActionError('')
      try {
        const result = await request<StoreDetail>(`${storeBase}/${collection()}/${id}/versions/${version}`, {
          method: 'PUT', headers: { 'content-type': 'application/webc' }, body: input,
          signal: AbortSignal.timeout(180_000),
        })
        if (result.id !== id || result.version !== version || !['published', 'yanked'].includes(result.status))
          throw new Error(t('apps.publishUnknown'))
        setFeedback(t(result.status === 'published' ? 'apps.publishSuccess' : 'apps.publishYanked'))
        setDialog(null)
        navigate(storeHref(id, version))
        void catalogActions.refetch()
      } catch (error) {
        setActionError(`${errorMessage(error, t('common.unexpectedError'))}${!(error instanceof ApiError) || error.status >= 500 ? ` · ${t('apps.publishUnknown')}` : ''}`)
      } finally { setWriting('idle') }
      return
    }
    const release = storeSelection()
    const id = release?.id ?? target()
    if (!validId(id)) {
      setActionError(t('apps.localOnly'))
      return
    }
    if (dialog() === 'file' && (!file() || !inspected() || !accepted())) return
    if (release && !accepted()) return
    setWriting('saving')
    setActionError('')
    try {
      const result = await request<LibSummary>(
        release ? '/std/sys/package/store-install' : `/std/sys/package/${collection()}/${encodeURIComponent(id.split('/').at(-1)!)}`,
        release ? { method: 'POST', body: JSON.stringify({ kind: release.kind ?? kind(), publisher: release.publisher, name: release.name, version: release.version }) } : dialog() === 'file'
          ? {
              method: 'PUT',
              headers: { 'content-type': 'application/webc' },
              body: file()!,
            }
          : { method: 'DELETE' },
      )
      if (release && ((result.kind ?? 'lib') !== (release.kind ?? kind()) || result.id !== id || result.version !== release.version))
        throw new Error(t('apps.verifyHint'))
      setFeedback(t(dialog() !== 'uninstall' ? 'apps.active' : 'apps.uninstalled'))
      setDismissedStateNotice('')
      setDialog(null)
      await Promise.all([diskActions.refetch(), loadedActions.refetch()])
    } catch (error) {
      setActionError(
        `${errorMessage(error, t('common.unexpectedError'))}${!(error instanceof ApiError) || error.status >= 500 ? ` · ${t('apps.verifyHint')}` : ''}`,
      )
      refresh()
    } finally {
      setWriting('idle')
    }
  }

  const accessActive = () =>
    detail() &&
    !!identity() &&
    identity()!.source === 'local' &&
    section() === 'access'
  const accessSource = () => (accessActive() ? identity()!.id : 'inactive')
  const [rules, rulesActions] = createResource(
    accessSource,
    abortable((source: string, signal) =>
      source === 'inactive'
        ? Promise.resolve(undefined)
        : api.rulesByApi(signal),
    ),
  )
  const [contacts] = createResource(
    accessSource,
    abortable((source: string, signal) =>
      source === 'inactive'
        ? Promise.resolve(undefined)
        : api.allContacts('name', 'asc', signal),
    ),
  )
  const savedRules = () => (rules.error ? undefined : rules.latest)
  const contactItems = () => (contacts.error ? undefined : contacts.latest)
  const ops = () => operations(currentRunning())
  const groups = () =>
    Object.entries(
      currentRunning()?.openapi?.['x-pishoo']?.['access-groups'] ?? {},
    ).filter(
      ([key, group]) =>
        group.operations.length > 0 &&
        new Set(group.operations).size === group.operations.length &&
        group.operations.every((id) => {
          const matches = ops().filter((op) => op.operation?.operationId === id)
          return (
            matches.length === 1 &&
            matches[0].operation?.['x-pishoo']?.['access-group'] === key
          )
        }),
    )
  const view = () =>
    ['feature', 'api'].includes(params().get('view') ?? '')
      ? params().get('view')!
      : 'visitor'
  const selection = () =>
    params().get('selected') ??
    (view() === 'feature'
      ? groups()[0]?.[0]
      : ops()[0]
        ? `${ops()[0].method} ${ops()[0].path}`
        : '')
  const selectedOps = () =>
    view() === 'feature'
      ? ops().filter((op) =>
          groups()
            .find(([key]) => key === selection())?.[1]
            .operations.includes(op.operation?.operationId ?? ''),
        )
      : ops().filter((op) => `${op.method} ${op.path}` === selection())
  const allRules = (): RuleRow[] =>
    Object.entries(savedRules() ?? {}).flatMap(([path, methods]) =>
      Object.entries(methods).flatMap(([method, effects]) =>
        (['allow', 'review', 'deny'] as const).flatMap((effect) =>
          (effects[effect] ?? []).map((grantee) => ({
            api: path,
            method,
            effect,
            grantee,
          })),
        ),
      ),
    )
  const appRules = () =>
    allRules().filter(
      (rule) =>
        rule.api === `/${kind() === 'app' ? 'app' : 'api'}/${identity()?.id?.split('/').at(-1)}` ||
        rule.api.startsWith(`/${kind() === 'app' ? 'app' : 'api'}/${identity()?.id?.split('/').at(-1)}/`),
    )
  const visitors = () =>
    [
      ...new Set([
        ...appRules().map((rule) => rule.grantee),
        ...(contactItems() ?? []).map((contact) => contact.name),
        ...(params().get('grantee') ? [params().get('grantee')!] : []),
      ]),
    ]
      .filter(
        (name) => !params().get('grantee') || name === params().get('grantee'),
      )
      .sort()
  const [editing, setEditing] = createSignal(false)
  const [grantee, setGrantee] = createSignal('')
  const [scope, setScope] = createSignal('')
  const [effect, setEffect] = createSignal<Effect | 'remove' | ''>('')
  const [savingRules, setSavingRules] = createSignal(false)
  const [ruleError, setRuleError] = createSignal('')
  const [results, setResults] = createSignal<
    Array<{
      rule: RuleRow
      result: 'success' | 'failed' | 'unknown' | 'notRun'
      message: string
    }>
  >([])
  const edit = (name = '', selected = '', savedEffect: Effect | '' = '') => {
    const precise = appRules().filter(
      (rule) =>
        rule.grantee === name &&
        ops().some((op) => op.path === rule.api && op.method === rule.method),
    )
    let selectedScope =
      selected ||
      (view() === 'feature'
        ? `group:${selection()}`
        : view() === 'api'
          ? `api:${selection()}`
          : '')
    let selectedEffect = savedEffect
    if (!selectedScope && name) {
      const reading = ops().filter((op) =>
        groups()
          .find(([key]) => key === 'read')?.[1]
          .operations.includes(op.operation?.operationId ?? ''),
      )
      const commenting = ops().filter((op) =>
        groups()
          .find(([key]) => key === 'comment')?.[1]
          .operations.includes(op.operation?.operationId ?? ''),
      )
      const matches = (items: ReturnType<typeof ops>, value: Effect) =>
        items.length > 0 &&
        items.every((op) => {
          const stored = precise.filter(
            (rule) => rule.api === op.path && rule.method === op.method,
          )
          return stored.length === 1 && stored[0].effect === value
        })
      if (matches(reading, 'allow') && matches(commenting, 'deny'))
        selectedScope = 'read-only'
      else if (matches(reading, 'allow') && matches(commenting, 'allow'))
        selectedScope = 'read-comment'
      else if (precise.length) {
        selectedScope = `api:${precise[0].method} ${precise[0].api}`
        selectedEffect = precise[0].effect
      }
    }
    setGrantee(name)
    setScope(selectedScope)
    setEffect(selectedEffect)
    setRuleError('')
    setResults([])
    setEditing(true)
  }
  const draft = () => {
    const selected = scope()
    const grouped = ops().filter((op) =>
      groups().some(([, group]) =>
        group.operations.includes(op.operation?.operationId ?? ''),
      ),
    )
    const choices =
      selected === 'read-only' || selected === 'read-comment'
        ? grouped.filter((op) =>
            ['read', 'comment'].includes(
              op.operation?.['x-pishoo']?.['access-group'] ?? '',
            ),
          )
        : selected === 'ungrant'
          ? grouped
          : selected.startsWith('group:')
            ? ops().filter((op) =>
                groups()
                  .find(([key]) => key === selected.slice(6))?.[1]
                  .operations.includes(op.operation?.operationId ?? ''),
              )
            : ops().filter((op) => `api:${op.method} ${op.path}` === selected)
    return choices.flatMap((op) => {
      const existing = allRules().filter(
        (rule) =>
          rule.grantee === grantee().trim() &&
          rule.api === op.path &&
          rule.method === op.method,
      )
      const nextEffect =
        selected === 'read-only'
          ? op.operation?.['x-pishoo']?.['access-group'] === 'comment'
            ? 'deny'
            : 'allow'
          : selected === 'read-comment'
            ? 'allow'
            : effect()
      if (selected === 'ungrant' || nextEffect === 'remove') {
        return existing.some(
          (rule) => selected !== 'ungrant' || rule.effect !== 'deny',
        ) &&
          (selected !== 'ungrant' ||
            !existing.some((rule) => rule.effect === 'deny'))
          ? [
              {
                rule: {
                  api: op.path,
                  method: op.method,
                  grantee: grantee().trim(),
                  effect: existing[0].effect,
                },
                remove: true,
              },
            ]
          : []
      }
      if (
        !nextEffect ||
        (existing.length === 1 && existing[0].effect === nextEffect)
      )
        return []
      return [
        {
          rule: {
            api: op.path,
            method: op.method,
            grantee: grantee().trim(),
            effect: nextEffect,
          },
          remove: false,
        },
      ]
    })
  }
  const saveRules = async (event: SubmitEvent) => {
    event.preventDefault()
    if (savingRules() || rules.loading || rules.error) return
    if (
      !scope() ||
      (!['read-only', 'read-comment', 'ungrant'].includes(scope()) && !effect())
    ) {
      setRuleError(t('apps.selectScope'))
      return
    }
    const changes = draft()
    if (!changes.length) {
      setRuleError(t('apps.noChanges'))
      return
    }
    setSavingRules(true)
    setRuleError('')
    const outcomes = changes.map((change) => ({
      rule: change.rule,
      result: 'notRun' as 'success' | 'failed' | 'unknown' | 'notRun',
      message: '',
    }))
    for (const [index, change] of changes.entries()) {
      try {
        await (change.remove
          ? api.deleteRule(change.rule)
          : api.setRule(change.rule))
        outcomes[index].result = 'success'
      } catch (error) {
        outcomes[index].result =
          error instanceof ApiError && error.status < 500 ? 'failed' : 'unknown'
        outcomes[index].message = errorMessage(
          error,
          t('common.unexpectedError'),
        )
        setRuleError(outcomes[index].message)
        break
      }
    }
    setResults([...outcomes])
    try {
      await rulesActions.refetch()
    } catch (error) {
      setRuleError(errorMessage(error, t('common.unexpectedError')))
    }
    if (rules.error)
      setRuleError(errorMessage(rules.error, t('common.unexpectedError')))
    if (outcomes.every((item) => item.result === 'success') && !rules.error)
      setFeedback(t('apps.rulesSaved'))
    setSavingRules(false)
  }
  createEffect(
    on(url, () => {
      if (!savingRules()) setEditing(false)
    }),
  )

  const secondaryActions = (id: string, showDetails = false) => (
    <>
      <Show when={id.includes('/')}>
        <a class="button button-secondary" href={storeHref(id)} onClick={event => follow(event, storeHref(id))}>
          {t('apps.store')}
        </a>
      </Show>
      <Show when={saved(id) && !disk.error}>
        <button class="button button-secondary" onClick={event => { event.currentTarget.closest('details')?.removeAttribute('open'); openFile(id) }}>
          {t('apps.update')}
        </button>
      </Show>
      <Show when={!saved(id) && disk.state === 'ready'}>
        <button class="button button-secondary" onClick={event => { event.currentTarget.closest('details')?.removeAttribute('open'); openFile(id) }}>
          {t('apps.file')}
        </button>
      </Show>
      <Show when={showDetails}>
        <a
          class="button button-secondary"
          href={detailHref(id)}
          onClick={(event) => follow(event, detailHref(id))}
        >
          {t('apps.details')}
        </a>
      </Show>
      <Show when={saved(id) && !disk.error}>
        <button
          class="button button-danger-quiet"
          onClick={event => { event.currentTarget.closest('details')?.removeAttribute('open'); openUninstall(id) }}
        >
          {t('apps.uninstall')}
        </button>
      </Show>
    </>
  )
  const actions = (id: string, showDetails = false) => (
    <div class="button-group app-actions">
      <Show when={entry(running(id))} keyed fallback={
        <button class="button button-primary" disabled title={t(entry(metadata(id)) ? 'apps.notRunning' : 'apps.noEntry')}>
          {t('apps.open')} ↗
        </button>
      }>
        {(href) => (
          <a
            class="button button-primary"
            href={href}
            target="_blank"
            rel="noopener noreferrer"
          >
            {t('apps.open')} ↗
          </a>
        )}
      </Show>
      <Show
        when={showDetails}
        fallback={
          <Show
            when={
              ((saved(id) && !disk.error) ||
                (!saved(id) && disk.state === 'ready'))
            }
          >
            <details class="app-more">
              <summary class="text-link">
                {t('apps.more')} <span aria-hidden="true">▾</span>
              </summary>
              <div class="app-more-menu">{secondaryActions(id)}</div>
            </details>
          </Show>
        }
      >
        {secondaryActions(id, true)}
      </Show>
    </div>
  )
  const storeActions = (release: StoreDetail) => (
    <div class="button-group app-actions">
      <Show when={entry(running(release.id))} keyed>
        {href => <a class="button button-secondary" href={href} target="_blank" rel="noopener noreferrer">{t('apps.open')} ↗</a>}
      </Show>
      <Show when={release.status === 'published'} fallback={<p class="app-notice">{t('apps.yanked')}: {release.status_reason}</p>}>
        <button class="button button-primary" disabled={writing() !== 'idle' || disk.loading || !!disk.error || disk.state !== 'ready'} onClick={() => openStoreInstall(release)}>
          {t('apps.installVersion', { version: release.version })}
        </button>
      </Show>
    </div>
  )
  const backendDependencies = (lib: LibSummary | undefined) => (
    <>
      <Show when={lib?.dependencies?.length} fallback={<p class="muted">{t('apps.noDependencies')}</p>}>
        <ul class="app-operations">
          <For each={lib?.dependencies}>{dependency => (
            <li>
              <a class="text-link" href={workspacePath(`/apps/${identity()?.source === 'store' ? `store/${encodeURIComponent(dependency.publisher)}` : 'local'}/${encodeURIComponent(dependency.name)}?kind=lib&version=${encodeURIComponent(dependency.version)}`)}>
                <code>{dependency.publisher}/{dependency.name}</code>
              </a>
              <p>v{dependency.version}</p>
            </li>
          )}</For>
        </ul>
      </Show>
    </>
  )
  const interfaces = (lib: LibSummary | undefined, links = false) => (
    <Show when={lib?.kind !== 'app'} fallback={backendDependencies(lib)}>
    <>
    <Show when={operations(lib).some(pageResource)}>
      <p class="muted app-page-resources">{t('apps.pageResources', { count: new Set(operations(lib).map(pageResource).filter(Boolean)).size })}</p>
    </Show>
    <ul class="app-operations">
      <For each={operations(lib).filter(op => !pageResource(op))}>
        {(op) => (
          <li>
            <span class="method">{op.method}</span>
            <Show
              when={
                links &&
                op.method === 'GET' &&
                validId(lib?.id ?? '') &&
                op.path.startsWith(`/${lib!.kind === 'app' ? 'app' : 'api'}/${lib!.id.split('/').at(-1)}/`)
              }
              fallback={<code>{op.path}</code>}
            >
              <a
                href={op.path}
                target="_blank"
                rel="noopener noreferrer"
                aria-label={t('apps.openApi', { path: op.path })}
              >
                <code>{op.path}</code> ↗
              </a>
            </Show>
            <p>{op.description}</p>
          </li>
        )}
      </For>
    </ul>
    <Show when={lib?.dependencies?.length}>
      <h3>{t('apps.dependencies')}</h3>
      {backendDependencies(lib)}
    </Show>
    </>
    </Show>
  )
  const suggestions = (lib: LibSummary | undefined) => (
    <Show
      when={operations(lib).some(
        (op) => op.operation?.['x-pishoo']?.['initial-access']?.length,
      )}
    >
      <details>
        <summary>{t('apps.initial')}</summary>
        <ul class="app-operations">
          <For each={operations(lib)}>
            {(op) => (
              <For each={op.operation?.['x-pishoo']?.['initial-access'] ?? []}>
                {(rule) => (
                  <li>
                    <code>{rule.grantee}</code> · {op.method}{' '}
                    <code>{op.path}</code> → {rule.effect}
                  </li>
                )}
              </For>
            )}
          </For>
        </ul>
      </details>
    </Show>
  )
  const sourceErrors = () => (
    <>
      <Show when={loaded.error}>
        <ErrorState
          message={`${t('apps.unknownRuntime')}: ${errorMessage(loaded.error, t('common.unexpectedError'))}`}
          onRetry={() => void loadedActions.refetch()}
        />
      </Show>
      <Show when={disk.error}>
        <ErrorState
          message={`${t('apps.unknownDisk')}: ${errorMessage(disk.error, t('common.unexpectedError'))}`}
          onRetry={() => void diskActions.refetch()}
        />
      </Show>
    </>
  )
  const storedRows = (rows: RuleRow[]) => (
    <Show
      when={rows.length}
      fallback={<p class="muted">{t('apps.noRules')}</p>}
    >
      <ul class="app-rule-list">
        <For each={rows}>
          {(rule) => (
            <li>
              <code>{rule.grantee}</code>
              <span>
                {rule.method} · {rule.effect}
              </span>
              <code>{rule.api}</code>
              <Show
                when={ops().some(
                  (op) => op.method === rule.method && op.path === rule.api,
                )}
                fallback={
                  <a
                    href={workspacePath(`/settings/access?api=${encodeURIComponent(rule.api)}`)}
                  >
                    {t('nav.policies')}
                  </a>
                }
              >
                <button
                  class="button button-secondary"
                  disabled={!currentRunning() || !!rules.error}
                  onClick={() =>
                    edit(
                      rule.grantee,
                      `api:${rule.method} ${rule.api}`,
                      rule.effect,
                    )
                  }
                >
                  {t('common.edit')}
                </button>
              </Show>
            </li>
          )}
        </For>
      </ul>
    </Show>
  )

  return (
    <div class="apps-page">
      <Show
        when={!detail()}
        fallback={
          <>
            <a
              class="text-link app-back"
              href={listHref()}
              onClick={(event) =>
                follow(event, listHref(), Number(params().get('scroll')) || 0)
              }
            >
              <span aria-hidden="true">←</span> {t('apps.back')}
            </a>
            <Show
              when={identity()}
              fallback={<ErrorState message={t('apps.invalidId')} />}
            >
              <Show
                when={identity()?.source === 'local' || selectedStore()}
                fallback={
                  <>
                    <Show when={storeDetail.loading}><LoadingState label={t('apps.loading')} /></Show>
                    <Show when={storeDetail.error}>
                      <ErrorState message={errorMessage(storeDetail.error, t('common.unexpectedError'))} onRetry={() => void storeDetailActions.refetch()} />
                    </Show>
                  </>
                }
              >
                {sourceErrors()}
                <Show when={(loaded.loading || disk.loading) && !current()}>
                  <LoadingState />
                </Show>
                <Show
                  when={current() || saved(identity()!.id)}
                  fallback={
                    <Show
                      when={
                        !loaded.loading &&
                        !disk.loading &&
                        !loaded.error &&
                        !disk.error
                      }
                    >
                      <EmptyState title={t('apps.missing')} />
                    </Show>
                  }
                >
                  <header class="app-detail-header">
                    <div class="app-identity">
                      <span class="app-icon" aria-hidden="true">
                        {(current()?.title || identity()!.id)
                          .slice(0, 1)
                          .toUpperCase()}
                      </span>
                      <div>
                        <h1>{current()?.title || identity()!.id}</h1>
                        <p class="app-meta">{publisher(current())}</p>
                        <p class="app-meta">v{current()?.version ?? '—'}</p>
                      </div>
                    </div>
                    <Show when={selectedStore()} fallback={actions(identity()!.id)} keyed>
                      {release => storeActions(release)}
                    </Show>
                  </header>
                  <Show when={detailNotice()}>
                    <div class="app-notice app-notice-dismissible" role="status">
                      <span>{detailNotice()}</span>
                      <button class="icon-button" type="button" aria-label={t('common.dismissMessage')} onClick={() => {
                        setDismissedStateNotice(stateNoticeKey())
                        setFeedback('')
                      }}>×</button>
                    </div>
                  </Show>
                  <nav class="section-nav app-tabs" aria-label={t('apps.details')}>
                    <For
                      each={
                        ([
                          'about',
                          'changes',
                          'interfaces',
                          'access',
                          'trust',
                        ] as const).filter(item => item !== 'access' || identity()?.source === 'local')
                      }
                    >
                      {(item) => (
                        <button
                          aria-current={section() === item ? 'page' : undefined}
                          classList={{ active: section() === item }}
                          onClick={() => changeQuery('section', item)}
                        >
                          {t(item === 'interfaces' && kind() === 'app' ? 'apps.dependencies' : `apps.${item}`)}
                        </button>
                      )}
                    </For>
                  </nav>
                  <Show when={section() === 'about'}>
                    <div class="app-about">
                      <section class="app-panel">
                        <h2>{t('apps.about')}</h2>
                        <p class="app-prose">
                          {current()?.description || t('apps.notProvided')}
                        </p>
                        <Show when={!entry(current())}>
                          <p class="app-notice">{t('apps.noEntry')}</p>
                        </Show>
                      </section>
                      <aside class="app-panel app-facts">
                        <h2>{t('apps.fullId')}</h2>
                        <code>{identity()!.id}</code>
                        <dl>
                          <dt>{t('apps.declared')}</dt>
                          <dd>{publisher(current())}</dd>
                          <dt>{t('apps.loaded')}</dt>
                          <dd>{running(identity()!.id)?.version ?? '—'}</dd>
                          <dt>{t('apps.savedVersion')}</dt>
                          <dd>
                            {(() => {
                              const value = saved(identity()!.id)
                              return value && !('error' in value)
                                ? value.version
                                : '—'
                            })()}
                          </dd>
                          <Show when={selectedStore()}>
                            <dt>{t('apps.chosenVersion')}</dt>
                            <dd>{selectedStore()!.version}</dd>
                          </Show>
                          <Show when={repositoryUrl(current()?.repository)} keyed>
                            {url => <>
                              <dt>{t('apps.repository')}</dt>
                              <dd><a class="text-link" href={url} target="_blank" rel="noopener noreferrer">{url}</a></dd>
                            </>}
                          </Show>
                          <dt>{t('apps.license')}</dt>
                          <dd>
                            {current()?.license ??
                              t('apps.notProvided')}
                          </dd>
                        </dl>
                      </aside>
                    </div>
                  </Show>
                  <Show when={section() === 'changes'}>
                    <section class="app-panel">
                      <h2>{t('apps.changes')}</h2>
                      <p class="app-prose">
                        {current()?.changelog ??
                          t('apps.notProvided')}
                      </p>
                      <Show when={identity()!.id.includes('/')}>
                        <Show when={history.loading}><LoadingState /></Show>
                        <Show when={history.error}>
                          <ErrorState message={errorMessage(history.error, t('common.unexpectedError'))} onRetry={() => void historyActions.refetch()} />
                        </Show>
                        <Show when={!history.error && history()}>
                          <ul class="app-operations">
                            <For each={history()?.versions ?? []}>
                              {release => <li>
                                <a href={storeHref(release.id, release.version, 'changes')} onClick={event => follow(event, storeHref(release.id, release.version, 'changes'))}>v{release.version}</a>
                                <p>{t(release.status === 'published' ? 'apps.published' : 'apps.yanked')} · {new Date(release.published_at * 1000).toLocaleString()}</p>
                                <p class="app-prose">{release.changelog ?? t('apps.notProvided')}</p>
                                <Show when={release.status_reason}><p>{release.status_reason}</p></Show>
                              </li>}
                            </For>
                          </ul>
                          <nav class="button-group app-actions" aria-label={t('apps.changes')}>
                            <button class="button button-secondary" disabled={historyPage() === 1 || history.loading} onClick={() => changeQuery('history_page', String(historyPage() - 1))}>{t('apps.historyPrevious')}</button>
                            <button class="button button-secondary" disabled={(history()?.versions.length ?? 0) < 20 || history.loading} onClick={() => changeQuery('history_page', String(historyPage() + 1))}>{t('apps.historyNext')}</button>
                          </nav>
                        </Show>
                      </Show>
                    </section>
                  </Show>
                  <Show when={section() === 'interfaces'}>
                    <section class="app-panel">
                      <h2>
                        {kind() === 'app' ? t('apps.dependencies') : t('apps.endpoints', {
                          count: operations(current()).filter(op => !pageResource(op)).length,
                        })}
                      </h2>
                      {interfaces(current(), !!currentRunning())}
                    </section>
                  </Show>
                  <Show when={section() === 'trust'}>
                    <section class="app-panel">
                      <h2>{t('apps.trust')}</h2>
                      <Show when={selectedStore()} keyed>
                        {release => <dl class="app-facts">
                          <dt>{t('apps.chosenVersion')}</dt><dd>{release.version}</dd>
                          <dt>{t('apps.publisherOwner')}</dt><dd><code>{release.publisher_owner_hash}</code></dd>
                          <dt>{t('apps.packageDigest')}</dt><dd><code>{release.package_sha256}</code></dd>
                          <dt>{t('apps.packageSize')}</dt><dd>{release.size.toLocaleString()} B</dd>
                          <dt>{t('apps.publishedAt')}</dt><dd>{new Date(release.published_at * 1000).toLocaleString()}</dd>
                        </dl>}
                      </Show>
                      <dl>
                        <For
                          each={Object.entries(
                            current()?.host ?? {},
                          )}
                        >
                          {([name, purpose]) => (
                            <>
                              <dt>
                                <code>{name}</code>
                              </dt>
                              <dd>{purpose}</dd>
                            </>
                          )}
                        </For>
                      </dl>
                    </section>
                  </Show>
                  <Show when={section() === 'access'}>
                    <Show
                      when={currentRunning()}
                      fallback={
                        <section class="app-panel">
                          <EmptyState title={t('apps.loadFirst')} />
                          {suggestions(current())}
                        </section>
                      }
                    >
                      <div class="app-access-toolbar">
                        <select
                          class="view-select"
                          aria-label={t('apps.access')}
                          value={view()}
                          onChange={(event) => {
                            const next = new URL(url())
                            next.searchParams.set('view', event.currentTarget.value)
                            next.searchParams.delete('selected')
                            navigate(next.pathname + next.search)
                          }}
                        >
                          <For each={['visitor', 'feature', 'api'] as const}>
                            {(item) => (
                              <option value={item}>
                                {t(
                                  item === 'visitor'
                                    ? 'apps.byVisitor'
                                    : item === 'feature'
                                      ? 'apps.byFeature'
                                      : 'apps.byApi',
                                )}
                              </option>
                            )}
                          </For>
                        </select>
                        <div class="app-access-actions">
                          <a
                            class="text-link"
                            href={workspacePath(`/settings/access?api=${encodeURIComponent(`/${kind() === 'app' ? 'app' : 'api'}/${identity()!.id.split('/').at(-1)}`)}`)}
                          >
                            {t('nav.policies')}
                          </a>
                          <a class="text-link" href={workspacePath("/approvals")}>
                            {t('nav.reviews')}
                          </a>
                          <button
                            class="button button-primary"
                            disabled={rules.loading || !!rules.error || !rules()}
                            onClick={() => edit(params().get('grantee') ?? '')}
                          >
                            {t('apps.grant')}
                          </button>
                        </div>
                      </div>
                      {suggestions(currentRunning())}
                      <Show when={rules.error}>
                        <ErrorState
                          message={errorMessage(
                            rules.error,
                            t('common.unexpectedError'),
                          )}
                          onRetry={() => void rulesActions.refetch()}
                        />
                      </Show>
                      <Show when={contacts.error}>
                        <ErrorState
                          message={errorMessage(
                            contacts.error,
                            t('common.unexpectedError'),
                          )}
                        />
                      </Show>
                      <Show when={rules.loading && !savedRules()}>
                        <LoadingState />
                      </Show>
                      <Show when={savedRules() && !rules.error}>
                        <Show
                          when={view() === 'visitor'}
                          fallback={
                            <div class="app-master-detail">
                              <aside class="app-panel app-selection">
                                <Show
                                  when={view() === 'feature'}
                                  fallback={
                                    <For each={ops()}>
                                      {(op) => (
                                        <button
                                          classList={{
                                            active:
                                              selection() ===
                                              `${op.method} ${op.path}`,
                                          }}
                                          onClick={() =>
                                            changeQuery(
                                              'selected',
                                              `${op.method} ${op.path}`,
                                            )
                                          }
                                        >
                                          <span class="method">
                                            {op.method}
                                          </span>{' '}
                                          <code>{op.path}</code>
                                        </button>
                                      )}
                                    </For>
                                  }
                                >
                                  <Show
                                    when={groups().length}
                                    fallback={<p>{t('apps.noGroups')}</p>}
                                  >
                                    <For each={groups()}>
                                      {([key, group]) => (
                                        <button
                                          classList={{
                                            active: selection() === key,
                                          }}
                                          onClick={() =>
                                            changeQuery('selected', key)
                                          }
                                        >
                                          {group.title}{' '}
                                          <span>{group.operations.length}</span>
                                        </button>
                                      )}
                                    </For>
                                  </Show>
                                </Show>
                              </aside>
                              <section class="app-panel">
                                <h2>
                                  {view() === 'feature'
                                    ? groups().find(
                                        ([key]) => key === selection(),
                                      )?.[1].title
                                    : t('apps.operations')}
                                </h2>
                                <For each={selectedOps()}>
                                  {(op) => (
                                    <div class="app-operation">
                                      <p>
                                        <span class="method">{op.method}</span>{' '}
                                        <code>{op.path}</code>
                                      </p>
                                      <p>{op.description}</p>
                                      {storedRows(
                                        appRules().filter(
                                          (rule) =>
                                            rule.api === op.path &&
                                            (rule.method === op.method ||
                                              rule.method === '*'),
                                        ),
                                      )}
                                    </div>
                                  )}
                                </For>
                              </section>
                            </div>
                          }
                        >
                          <section class="app-panel">
                            <Show
                              when={visitors().length}
                              fallback={
                                <EmptyState title={t('apps.noRules')} />
                              }
                            >
                              <div class="app-visitors">
                                <For each={visitors()}>
                                  {(name) => (
                                    <article>
                                      <code>{name}</code>
                                      <div>
                                        <span class="muted">
                                          {t('apps.explicit')}
                                        </span>
                                        <Show
                                          when={appRules().some(
                                            (rule) => rule.grantee === name,
                                          )}
                                          fallback={<p>{t('apps.noRules')}</p>}
                                        >
                                          <For
                                            each={appRules().filter(
                                              (rule) => rule.grantee === name,
                                            )}
                                          >
                                            {(rule) => (
                                              <p>
                                                <span class="method">
                                                  {rule.method}
                                                </span>{' '}
                                                <code>{rule.api}</code> ·{' '}
                                                {rule.effect}
                                              </p>
                                            )}
                                          </For>
                                        </Show>
                                      </div>
                                      <button
                                        class="button button-secondary"
                                        onClick={() => edit(name)}
                                      >
                                        {t('common.edit')}
                                      </button>
                                    </article>
                                  )}
                                </For>
                              </div>
                            </Show>
                          </section>
                        </Show>
                      </Show>
                    </Show>
                  </Show>
                </Show>
              </Show>
            </Show>
          </>
        }
      >
        <header class="page-header">
          <h1>{t('nav.apps')}</h1>
          <button class="button button-primary" disabled={writing() !== 'idle'} onClick={() => tab() === 'store' ? openFile('', 'publish') : openFile()}>
            {t(tab() === 'store' ? 'apps.publish' : 'apps.file')}
          </button>
        </header>
        <nav class="section-nav app-tabs" aria-label={t('nav.apps')}>
          <For each={['mine', 'store'] as const}>
            {(item) => (
              <button
                classList={{ active: tab() === item }}
                aria-current={tab() === item ? 'page' : undefined}
                onClick={() => changeQuery('tab', item)}
              >
                {t(`apps.${item}`)}
              </button>
            )}
          </For>
        </nav>
        <form
          class="app-search"
          onSubmit={(event) => {
            event.preventDefault()
            const data = new FormData(event.currentTarget)
            const next = new URL(url())
            next.searchParams.set('q', String(data.get('q') ?? ''))
            next.searchParams.delete('page')
            if (tab() === 'store')
              next.searchParams.set(
                'publisher',
                String(data.get('publisher') ?? ''),
              )
            navigate(next.pathname + next.search)
          }}
        >
          <select aria-label={t('apps.artifactKind')} value={kind()} disabled={writing() !== 'idle'} onChange={event => changeQuery('kind',event.currentTarget.value)}>
            <option value="app">{t('apps.frontend')}</option>
            <option value="lib">{t('apps.backend')}</option>
          </select>
          <input
            name="q"
            type="search"
            aria-label={t('apps.search')}
            placeholder={t('apps.search')}
            value={params().get('q') ?? ''}
            maxLength={256}
          />
          <Show when={tab() === 'store'}>
            <input
              name="publisher"
              aria-label={t('apps.publisher')}
              placeholder={t('apps.publisher')}
              value={params().get('publisher') ?? ''}
            />
          </Show>
          <button class="button button-secondary" type="submit">
            {t('apps.search')}
          </button>
          <button
            class="button button-secondary"
            type="button"
            disabled={disk.loading || !!disk.error || updates.loading}
            onClick={checkUpdates}
          >
            {t('apps.check')}
          </button>
          <Show when={tab() === 'mine'}>
            <button
              class="button button-secondary"
              type="button"
              disabled={loaded.loading || disk.loading}
              onClick={refresh}
            >
              {t('common.refresh')}
            </button>
          </Show>
        </form>
        <Show when={feedback()}>
          <div class="app-notice app-notice-dismissible" role="status">
            <span>{feedback()}</span>
            <button class="icon-button" type="button" aria-label={t('common.dismissMessage')} onClick={() => setFeedback('')}>×</button>
          </div>
        </Show>
        <Show when={tab() === 'store'}>
          {sourceErrors()}
          <Show when={catalog.loading}><LoadingState label={t('apps.loading')} /></Show>
          <Show when={catalog.error}>
            <ErrorState message={errorMessage(catalog.error, t('common.unexpectedError'))} onRetry={() => void catalogActions.refetch()} />
          </Show>
          <Show when={!catalog.loading && !catalog.error && catalog()}>
            <Show when={catalog()!.libs.length} fallback={<section class="app-panel"><EmptyState
              title={t(params().get('q') || params().get('publisher') ? 'apps.noMatch' : 'apps.storeEmpty')}
            /></section>}>
              <section class="app-grid" aria-label={t('apps.store')}>
                <For each={catalog()!.libs}>
                  {release => <article class="app-card">
                    <a class="app-card-title" href={storeHref(release.id, release.version)} onClick={event => follow(event, storeHref(release.id, release.version))}>
                      <span class="app-icon" aria-hidden="true">{(release.title || release.name).slice(0, 1).toUpperCase()}</span>
                      <h2>{release.title || release.name}</h2>
                    </a>
                    <p class="app-meta">{release.publisher}</p>
                    <p class="app-description">{release.description ?? t('apps.notProvided')}</p>
                    <p class="app-meta">v{release.version} · {t(release.kind === 'app' ? 'apps.frontend' : 'apps.backend')}</p>
                    <Show when={saved(release.id) || running(release.id)}>
                      <p class="app-meta">{t('apps.savedVersion')}: {(() => {
                        const lib = saved(release.id)
                        return lib && !('error' in lib) ? lib.version : '—'
                      })()}</p>
                      <p class="app-notice">{stateText(release.id)}</p>
                    </Show>
                    <div class="button-group app-actions">
                      <button class="button button-primary" disabled={writing() !== 'idle' || disk.loading || !!disk.error || disk.state !== 'ready'} onClick={() => void prepareStoreInstall(release)}>
                        {t('apps.installVersion', { version: release.version })}
                      </button>
                      <a class="button button-secondary" href={storeHref(release.id, release.version)} onClick={event => follow(event, storeHref(release.id, release.version))}>{t('apps.details')}</a>
                    </div>
                  </article>}
                </For>
              </section>
            </Show>
            <Pagination page={catalog()!.page} pageSize={catalog()!.page_size} total={catalog()!.total} onPage={next => changeQuery('page', String(next))} />
          </Show>
        </Show>
        <Show when={tab() === 'mine' && params().get('updates') === '1'}>
          <Show when={updates.loading}><p class="muted" role="status">{t('apps.check')}…</p></Show>
          <Show when={updates.error}><p class="field-error" role="alert">{t('apps.checkFailed')}</p></Show>
          <Show when={updateInputs() && !updates.loading && !updates.error && updates()?.some(result => result.status === 'update')}>
            <ul class="app-update-list" aria-label={t('apps.updateResults')}>
              <For each={updates()?.filter(result => result.status === 'update')}>
                {result => {
                  if (result.status !== 'update') return null
                  return <li>
                    <div>
                      <a class="text-link" href={storeHref(result.id, result.version)} onClick={event => follow(event, storeHref(result.id, result.version))}>
                        {metadata(result.id)?.title ?? result.id}
                      </a>
                      <p class="app-meta">{t('apps.updateAvailable', { version: result.version })}</p>
                      <Show when={result.changelog}><p class="app-prose">{result.changelog}</p></Show>
                    </div>
                    <button class="button button-primary" disabled={writing() !== 'idle' || disk.loading || !!disk.error} onClick={() => void prepareStoreInstall(result)}>{t('apps.updateAvailable', { version: result.version })}</button>
                  </li>
                }}
              </For>
            </ul>
          </Show>
        </Show>
        <Show when={tab() === 'mine'}>
          {sourceErrors()}
          <Show when={(loaded.loading || disk.loading) && !ids().length}>
            <LoadingState label={t('apps.loading')} />
          </Show>
          <Show
            when={
              !loaded.loading &&
              !disk.loading &&
              !ids().length &&
              !loaded.error &&
              !disk.error
            }
          >
            <section class="app-panel">
              <EmptyState
                title={t('apps.noApps')}
              />
              <button
                class="button button-secondary"
                onClick={() => changeQuery('tab', 'store')}
              >
                {t('apps.store')}
              </button>
            </section>
          </Show>
          <Show when={ids().length > 0 && filtered().length === 0}>
            <EmptyState title={t('apps.noMatch')} />
            <button
              class="button button-secondary"
              onClick={() => changeQuery('q', '')}
            >
              {t('apps.clear')}
            </button>
          </Show>
          <section class="app-grid" aria-label={t('apps.mine')}>
            <For each={filtered().slice((page() - 1) * 20, page() * 20)}>
              {(id) => (
                <article class="app-card">
                  <a
                    class="app-card-title"
                    href={detailHref(id)}
                    onClick={(event) => follow(event, detailHref(id))}
                  >
                    <span class="app-icon" aria-hidden="true">
                      {(metadata(id)?.title || id).slice(0, 1).toUpperCase()}
                    </span>
                    <h2>{metadata(id)?.title || id}</h2>
                  </a>
                  <p class="app-meta">{publisher(metadata(id))}</p>
                  <p class="app-description">
                    {metadata(id)?.description ?? t('apps.notProvided')}
                  </p>
                  <p class="app-meta">v{metadata(id)?.version ?? '—'} · {t(kind() === 'app' ? 'apps.frontend' : 'apps.backend')}</p>
                  <Show when={stateText(id)}>
                    <p class="app-notice">{stateText(id)}</p>
                  </Show>
                  {actions(id, true)}
                </article>
              )}
            </For>
          </section>
          <Show when={filtered().length > 0}>
            <Pagination
              page={page()}
              pageSize={20}
              total={filtered().length}
              onPage={(next) => changeQuery('page', String(next))}
            />
          </Show>
        </Show>
      </Show>
      <Dialog
        open={dialog() !== null}
        title={t(dialog() === 'publish' ? 'apps.publish' : dialog() === 'file' ? 'apps.file' : dialog() === 'uninstall' ? 'apps.confirmUninstall' : 'apps.storeConfirm')}
        wide
        onClose={() => writing() === 'idle' && setDialog(null)}
      >
        <form class="form-grid app-install" onSubmit={saveComponent}>
          <p class="field-wide">
            {t('apps.target')}:{' '}
            <code>
              {(context.error ? undefined : context()?.owner_name) ??
                t('app.connecting')}
            </code>
          </p>
          <Show
            when={dialog() !== 'uninstall'}
            fallback={
              <>
                <h3 class="field-wide">
                  {metadata(target())?.title ?? target()}
                </h3>
                <code class="field-wide">{target()}</code>
                <p class="field-wide">{t('apps.uninstallHint')}</p>
              </>
            }
          >
            <Show when={dialog() === 'file' || dialog() === 'publish'}>
            <Show when={dialog() === 'file'}>
            <label class="field field-wide">
              <span>{t('apps.fileId')}</span>
              <input
                value={target()}
                required
                pattern="(?:[a-z0-9.-]+/)?[a-z][a-z0-9-]{0,62}"
                disabled={writing() !== 'idle'}
                onInput={(event) => {
                  setTarget(event.currentTarget.value)
                  setAccepted(false)
                }}
              />
            </label>
            </Show>
            <label class="field field-wide">
              <span>{t('apps.component')}</span>
              <input
                type="file"
                accept=".webc,application/webc"
                required
                disabled={writing() !== 'idle'}
                onChange={(event) => {
                  setFile(event.currentTarget.files?.[0])
                  setInspected(undefined)
                  setAccepted(false)
                  setActionError('')
                }}
              />
            </label>
            <button
              class="button button-secondary"
              type="button"
              disabled={!file() || writing() !== 'idle'}
              onClick={() => void inspectFile()}
            >
              {t('apps.fileCheck')}
            </button>
            </Show>
            <Show when={installPreview()}>
              <section class="field-wide app-panel">
                <h3>
                  {installPreview()!.title} · v{installPreview()!.version}
                </h3>
                <code>{installId()}</code>

                <p>{publisher(installPreview())}</p>
                <p class="app-prose">
                  {installPreview()!.changelog ??
                    installPreview()!.description}
                </p>
                <Show when={dialog() !== 'publish'}><p>
                  {t('apps.savedVersion')}:{' '}
                  {(() => {
                    const value = saved(installId())
                    return value && !('error' in value) ? value.version : '—'
                  })()}
                </p></Show>
                <details class="app-preview-interfaces">
                  <summary>{installPreview()?.kind === 'app' ? t('apps.dependencies') : t('apps.endpoints', { count: operations(installPreview()).filter(op => !pageResource(op)).length })}</summary>
                  {interfaces(installPreview())}
                </details>
                {suggestions(installPreview())}
                <Show when={dialog() !== 'publish' && metadata(installId())}>
                  <details>
                    <summary>{t('apps.compare')}</summary>
                    <ul class="app-operations">
                      <For
                        each={manifestChanges(metadata(installId()), installPreview())}
                      >
                        {(change) => (
                          <li>
                            <code>{change.key}</code>
                            <p>
                              {t('apps.before')}: {change.before ?? '—'}
                            </p>
                            <p>
                              {t('apps.after')}: {change.after ?? '—'}
                            </p>
                          </li>
                        )}
                      </For>
                    </ul>
                  </details>
                </Show>
                <dl>
                  <For
                    each={Object.entries(
                      installPreview()!.host ?? {},
                    )}
                  >
                    {([name, purpose]) => (
                      <>
                        <dt>{name}</dt>
                        <dd>{purpose}</dd>
                      </>
                    )}
                  </For>
                </dl>
                <Show when={storeSelection()} keyed>
                  {release => <dl class="app-facts">
                    <dt>{t('apps.packageDigest')}</dt><dd><code>{release.package_sha256}</code></dd>
                    <dt>{t('apps.publisherOwner')}</dt><dd><code>{release.publisher_owner_hash}</code></dd>
                  </dl>}
                </Show>
              </section>
            </Show>
            <Show when={dialog() !== 'publish'}>
              <p class="field-wide">{t(storeSelection() ? 'apps.storeInstallHint' : 'apps.fileHint')}</p>
            </Show>
            <Show when={installPreview()?.host?.signatures}>
              <p class="app-notice field-wide">{t('apps.signatureCapability')}</p>
            </Show>
            <Show when={storeSelection()} keyed>
              {release => <p class="field-wide">{t('apps.chosenVersion')}: <code>{release.id}</code> · v{release.version}</p>}
            </Show>
            <label class="app-checkbox field-wide">
              <input
                type="checkbox"
                checked={accepted()}
                onChange={(event) => setAccepted(event.currentTarget.checked)}
                disabled={writing() !== 'idle'}
              />
              {t(dialog() === 'publish' ? 'apps.publishAccept' : 'apps.accept')}
            </label>
          </Show>
          <Show when={actionError()}>
            <p class="field-error field-wide" role="alert">
              {actionError()}
            </p>
          </Show>
          <div class="dialog-actions field-wide">
            <button
              class="button button-secondary"
              type="button"
              disabled={writing() !== 'idle'}
              onClick={() => setDialog(null)}
            >
              {t('common.cancel')}
            </button>
            <button
              class={`button ${dialog() !== 'uninstall' ? 'button-primary' : 'button-danger'}`}
              type="submit"
              disabled={
                writing() !== 'idle' ||
                (dialog() !== 'uninstall' && (!accepted() || !installPreview()))
                || !!storeSelection() && (disk.loading || !!disk.error || !!context.error || !context()?.owner_name)
                || dialog() === 'publish' && (!!context.error || !context()?.owner_name)
              }
            >
              {writing() !== 'idle'
                ? t('common.saving')
                : t(
                    dialog() === 'publish' ? 'apps.publishConfirm' : dialog() !== 'uninstall'
                      ? saved(installId())
                        ? 'apps.confirmUpdate'
                        : 'apps.confirmInstall'
                      : 'apps.confirmUninstall',
                  )}
            </button>
          </div>
        </form>
      </Dialog>
      <Dialog
        open={editing()}
        title={t('apps.grant')}
        wide
        onClose={() => !savingRules() && setEditing(false)}
      >
        <form class="form-grid" onSubmit={saveRules}>
          <label class="field field-wide">
            <span>{t('policies.grantee')}</span>
            <input
              list="app-grantees"
              required
              value={grantee()}
              disabled={savingRules()}
              onInput={(event) => setGrantee(event.currentTarget.value)}
            />
          </label>
          <datalist id="app-grantees">
            <option value="**" />
            <option value="?" />
            <option value="*?" />
            <For each={contactItems() ?? []}>
              {(contact) => (
                <option value={contact.name}>
                  {contact.alias ?? contact.name}
                </option>
              )}
            </For>
          </datalist>
          <label class="field field-wide">
            <span>{t('apps.scope')}</span>
            <select
              aria-label={t('apps.scope')}
              value={scope()}
              disabled={savingRules()}
              onChange={(event) => {
                setScope(event.currentTarget.value)
                setEffect('')
              }}
            >
              <option value="">{t('apps.choose')}</option>
              <Show
                when={
                  groups().some(([key]) => key === 'read') &&
                  groups().some(([key]) => key === 'comment')
                }
              >
                <option value="read-only">{t('apps.readOnly')}</option>
                <option value="read-comment">{t('apps.readComment')}</option>
              </Show>
              <Show when={groups().length}>
                <option value="ungrant">{t('apps.ungrant')}</option>
              </Show>
              <For each={groups()}>
                {([key, group]) => (
                  <option value={`group:${key}`}>{group.title}</option>
                )}
              </For>
              <For each={ops()}>
                {(op) => (
                  <option value={`api:${op.method} ${op.path}`}>
                    {op.method} {op.path}
                  </option>
                )}
              </For>
            </select>
          </label>
          <Show
            when={!['read-only', 'read-comment', 'ungrant'].includes(scope())}
          >
            <label class="field field-wide">
              <span>{t('policies.effect')}</span>
              <select
                aria-label={t('policies.effect')}
                value={effect()}
                disabled={savingRules()}
                onChange={(event) =>
                  setEffect(event.currentTarget.value as Effect | 'remove' | '')
                }
              >
                <option value="">{t('apps.choose')}</option>
                <For each={['allow', 'review', 'deny'] as const}>
                  {(value) => (
                    <option value={value}>
                      {t(`status.${value}`)} ({value})
                    </option>
                  )}
                </For>
                <option value="remove">{t('apps.removeExact')}</option>
              </select>
            </label>
          </Show>
          <details class="field-wide" open>
            <summary>
              {t('apps.preview')} ({draft().length})
            </summary>
            <ul class="app-operations">
              <For each={draft()}>
                {(change) => (
                  <li>
                    <code>{change.rule.grantee}</code> · {change.rule.method}{' '}
                    <code>{change.rule.api}</code> →{' '}
                    {change.remove ? 'DELETE' : change.rule.effect}
                  </li>
                )}
              </For>
            </ul>
          </details>
          <Show when={ruleError()}>
            <p class="field-error field-wide" role="alert">
              {ruleError()}
            </p>
          </Show>
          <Show when={results().length}>
            <section class="field-wide" aria-label={t('apps.partial')}>
              <h3>{t('apps.partial')}</h3>
              <ul class="app-operations">
                <For each={results()}>
                  {(item) => (
                    <li>
                      {item.rule.method} <code>{item.rule.api}</code> ·{' '}
                      {t(`apps.${item.result}`)}
                      <p>{item.message}</p>
                    </li>
                  )}
                </For>
              </ul>
            </section>
          </Show>
          <div class="dialog-actions field-wide">
            <button
              class="button button-secondary"
              type="button"
              disabled={savingRules()}
              onClick={() => setEditing(false)}
            >
              {t('common.cancel')}
            </button>
            <button
              class="button button-primary"
              type="submit"
              disabled={savingRules() || rules.loading || !!rules.error}
            >
              {savingRules() ? t('common.saving') : t('policies.save')}
            </button>
          </div>
        </form>
      </Dialog>
    </div>
  )
}
