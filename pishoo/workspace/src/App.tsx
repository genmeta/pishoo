import { For, Match, Show, Switch, createEffect, createResource, createSignal, onCleanup, onMount } from 'solid-js'

import { api } from './api/client'
import type { ProfileSettings } from './api/types'
import Sidebar from './components/Sidebar'
import { Toast } from './components/Ui'
import WorkspaceChatBox from './components/WorkspaceChatBox'
import type { MessageKey } from './i18n'
import { useI18n } from './i18n'
import { abortable } from './lib/abortable'
import { displayIdentityName, profileAvatarUrl } from './lib/format'
import { routeFromPath } from './lib/routes'
import type { WorkspacePage } from './lib/routes'
import ContactsPage from './pages/ContactsPage'
import ChatPage from './pages/ChatPage'
import AddContactPage from './pages/AddContactPage'
import AppsPage from './pages/AppsPage'
import CapabilitiesPage from './pages/CapabilitiesPage'
import HomePage from './pages/HomePage'
import OutboundRequestsPage from './pages/OutboundRequestsPage'
import PoliciesPage from './pages/PoliciesPage'
import ProfileSettingsPage from './pages/ProfileSettingsPage'
import ReviewsPage from './pages/ReviewsPage'

type ToastState = { message: string; tone: 'success' | 'error' } | null
type NavItem = { page: WorkspacePage; href: string; label: MessageKey }

const SETTINGS_NAV: NavItem[] = [
  { page: 'profile', href: '/workspace/settings/profile', label: 'nav.profile' },
  { page: 'capabilities', href: '/workspace/settings/capabilities', label: 'nav.capabilities' },
  { page: 'access', href: '/workspace/settings/access', label: 'nav.policies' },
]

const PAGE_TITLES: Record<WorkspacePage, MessageKey> = {
  home: 'home.title', contacts: 'nav.contacts', 'contact-new': 'nav.contactNew',
  'contact-requests': 'nav.contactRequests', extensions: 'nav.extensions', apps: 'nav.apps',
  chat: 'chat.title', approvals: 'nav.reviews', profile: 'nav.profile',
  access: 'nav.policies', capabilities: 'nav.capabilities', 'not-found': 'page.notFound',
}

function primaryPage(page: WorkspacePage): WorkspacePage {
  if (page === 'contacts' || page === 'contact-new' || page === 'contact-requests') return 'contacts'
  if (page === 'chat') return 'contacts'
  if (page === 'profile' || page === 'access' || page === 'capabilities') return 'profile'
  return page
}

export default function App() {
  const { t } = useI18n()
  const [context, contextActions] = createResource(api.context)
  const loadProfile = abortable((_source: boolean, signal: AbortSignal) => api.profileSettings(signal))
  const [profile, profileActions] = createResource(() => true, loadProfile)
  const currentPath = () => `${window.location.pathname}${window.location.search}`
  const [pathname, setPathname] = createSignal(currentPath())
  const route = () => routeFromPath(pathname().split('?')[0])
  const [toast, setToast] = createSignal<ToastState>(null)
  let toastTimer: number | undefined

  const notify = (message: string, tone: 'success' | 'error' = 'success') => {
    window.clearTimeout(toastTimer)
    setToast({ message, tone })
    toastTimer = window.setTimeout(() => setToast(null), 4500)
  }

  const navigate = (next: string) => {
    if (next === currentPath()) return
    window.history.pushState({}, '', next)
    setPathname(currentPath())
    window.scrollTo(0, 0)
  }

  const follow = (event: MouseEvent, href: string) => {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return
    event.preventDefault()
    navigate(href)
  }

  const onPopState = () => setPathname(currentPath())
  const pendingCount = () => (context()?.badges.pending_reviews ?? 0) + (context()?.badges.incoming_contacts ?? 0)
  const onVisibilityChange = () => {
    if (!document.hidden) void contextActions.refetch()
  }
  onMount(() => {
    window.addEventListener('popstate', onPopState)
    document.addEventListener('visibilitychange', onVisibilityChange)
  })
  onCleanup(() => {
    window.removeEventListener('popstate', onPopState)
    document.removeEventListener('visibilitychange', onVisibilityChange)
    window.clearTimeout(toastTimer)
  })

  createEffect(() => {
    document.title = `${t(PAGE_TITLES[route().page])} · pishoo Workspace`
  })

  const navLink = (item: NavItem, active: () => boolean) => (
    <a href={item.href} classList={{ active: active() }} aria-current={active() ? 'page' : undefined}
      onClick={(event) => follow(event, item.href)}>
      {t(item.label)}
    </a>
  )

  const ownerName = () => context()?.owner_name ?? context()?.profile ?? t('app.connecting')
  const profileLabel = () => profile()?.display_name || displayIdentityName(ownerName())
  const profileAvatar = () => profileAvatarUrl(profile())

  return (
    <div class="app-shell" classList={{ 'app-shell-chat': route().page === 'chat' }}>
      <a class="skip-link" href="#main-content">{t('nav.skipContent')}</a>
      <Sidebar activePage={primaryPage(route().page)} profileName={profileLabel()}
        profileIdentity={displayIdentityName(ownerName())} profileAvatar={profileAvatar()}
        pendingCount={pendingCount()} follow={follow} />

      <main class="workspace" id="main-content" tabindex="-1">
        <Show when={primaryPage(route().page) === 'profile'}>
          <nav class="section-nav" aria-label={t('nav.section')}>
            <For each={SETTINGS_NAV}>{(item) => navLink(item, () => route().page === item.page)}</For>
          </nav>
        </Show>

        <Switch>
          <Match when={route().page === 'home'}>
            <HomePage ownerName={ownerName()} displayName={profile()?.display_name ?? null}
              avatarUrl={profileAvatar()}
              pendingCount={pendingCount()} pendingLoading={context.loading}
              pendingError={context.error} follow={follow} />
          </Match>
          <Match when={route().page === 'contacts'}>
            <ContactsPage notify={notify} contactName={route().contactName} navigate={navigate}
              returnTo={new URLSearchParams(pathname().split('?')[1] ?? '').get('from') === 'approvals'
                ? '/workspace/approvals' : undefined}
              onResolved={() => void contextActions.refetch()} />
          </Match>
          <Match when={route().page === 'contact-requests'}>
            <OutboundRequestsPage notify={notify} />
          </Match>
          <Match when={route().page === 'chat'}>
            <ChatPage contactName={route().contactName!} navigate={navigate} notify={notify} />
          </Match>
          <Match when={route().page === 'approvals'}>
            <ReviewsPage notify={notify} onRefresh={() => void contextActions.refetch()} />
          </Match>
          <Match when={route().page === 'access'}><PoliciesPage /></Match>
          <Match when={route().page === 'capabilities'}><CapabilitiesPage /></Match>
          <Match when={route().page === 'profile'}>
            <ProfileSettingsPage profile={profile()} loading={profile.loading} error={profile.error}
              ownerName={ownerName()} refresh={() => void profileActions.refetch()}
              onSaved={(value: ProfileSettings) => profileActions.mutate(value)} notify={notify} />
          </Match>
          <Match when={route().page === 'apps'}><AppsPage /></Match>
          <Match when={route().page === 'extensions'}>
            <header class="page-header"><h1>{t(PAGE_TITLES[route().page])}</h1></header>
            <section class="overview-panel placeholder-panel" role="status">
              <h2>{t('page.notEnabled')}</h2>
            </section>
          </Match>
          <Match when={route().page === 'contact-new'}><AddContactPage notify={notify} navigate={navigate} /></Match>
          <Match when={route().page === 'not-found'}>
            <header class="page-header"><h1>{t('page.notFound')}</h1></header>
            <a href="/workspace/" onClick={(event) => follow(event, '/workspace/')}>{t('page.backHome')}</a>
          </Match>
        </Switch>
      </main>

      <Show when={route().page === 'home'}>
        <WorkspaceChatBox />
      </Show>

      <Toast message={toast()?.message ?? null} tone={toast()?.tone ?? 'success'} onClose={() => setToast(null)} />
    </div>
  )
}
