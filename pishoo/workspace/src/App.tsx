import { Match, Show, Switch, createEffect, createResource, createSignal, onCleanup, onMount } from 'solid-js'

import { api } from './api/client'
import { Toast } from './components/Ui'
import type { MessageKey } from './i18n'
import { useI18n } from './i18n'
import { displayIdentityName } from './lib/format'
import ContactsPage from './pages/ContactsPage'
import PoliciesPage from './pages/PoliciesPage'
import ReviewsPage from './pages/ReviewsPage'

type Section = 'reviews' | 'contacts' | 'policies'
type ToastState = { message: string; tone: 'success' | 'error' } | null

const SECTIONS: Section[] = ['reviews', 'contacts', 'policies']
const SECTION_LABELS: Record<Section, MessageKey> = {
  reviews: 'nav.reviews',
  contacts: 'nav.contacts',
  policies: 'nav.policies',
}
const ACCESS_ROOT = '/workspace/access'

function sectionFromLocation(): Section {
  const path = window.location.pathname
  const segment = path.startsWith(`${ACCESS_ROOT}/`)
    ? path.slice(ACCESS_ROOT.length + 1).split('/')[0]
    : ''
  return SECTIONS.includes(segment as Section) ? (segment as Section) : 'reviews'
}

export default function App() {
  const { locale, setLocale, t } = useI18n()
  const [context] = createResource(api.context)
  const [section, setSection] = createSignal<Section>(sectionFromLocation())
  const [toast, setToast] = createSignal<ToastState>(null)
  let toastTimer: number | undefined

  const notify = (message: string, tone: 'success' | 'error' = 'success') => {
    window.clearTimeout(toastTimer)
    setToast({ message, tone })
    toastTimer = window.setTimeout(() => setToast(null), 4500)
  }

  const navigate = (next: Section) => {
    if (next === section()) return
    window.history.pushState({}, '', `${ACCESS_ROOT}/${next}`)
    setSection(next)
  }

  const onPopState = () => setSection(sectionFromLocation())
  onMount(() => window.addEventListener('popstate', onPopState))
  onCleanup(() => {
    window.removeEventListener('popstate', onPopState)
    window.clearTimeout(toastTimer)
  })

  createEffect(() => {
    document.title = `${t(SECTION_LABELS[section()])} · pishoo Workspace`
  })

  return (
    <div class="app-shell">
      <aside class="sidebar">
        <div class="brand">
          <span class="brand-mark" aria-hidden="true">P</span>
          <span>
            <strong>pishoo</strong>
            <small>{t('brand.subtitle')}</small>
          </span>
        </div>

        <div class="language-switch" role="group" aria-label={t('language.label')}>
          <button
            type="button"
            aria-label={t('language.english')}
            aria-pressed={locale() === 'en'}
            classList={{ active: locale() === 'en' }}
            onClick={() => setLocale('en')}
          >
            EN
          </button>
          <button
            type="button"
            aria-label={t('language.chinese')}
            aria-pressed={locale() === 'zh-CN'}
            classList={{ active: locale() === 'zh-CN' }}
            onClick={() => setLocale('zh-CN')}
          >
            中文
          </button>
        </div>

        <nav class="primary-nav" aria-label={t('nav.primary')}>
          {SECTIONS.map((item) => (
            <button
              type="button"
              classList={{ active: section() === item }}
              aria-current={section() === item ? 'page' : undefined}
              onClick={() => navigate(item)}
            >
              {t(SECTION_LABELS[item])}
            </button>
          ))}
        </nav>

        <div class="profile-summary">
          <span class="presence" aria-hidden="true" />
          <span>
            <strong>
              {displayIdentityName(context()?.owner_name ?? context()?.profile ?? t('app.connecting'))}
            </strong>
          </span>
          <Show when={context()?.development_identity}>
            <span class="environment-badge">{context()?.demo_data ? 'DEMO' : 'DEV'}</span>
          </Show>
        </div>
      </aside>

      <main class="workspace">
        <Switch>
          <Match when={section() === 'reviews'}>
            <ReviewsPage notify={notify} />
          </Match>
          <Match when={section() === 'contacts'}>
            <ContactsPage notify={notify} />
          </Match>
          <Match when={section() === 'policies'}>
            <PoliciesPage notify={notify} />
          </Match>
        </Switch>
      </main>

      <Toast
        message={toast()?.message ?? null}
        tone={toast()?.tone ?? 'success'}
        onClose={() => setToast(null)}
      />
    </div>
  )
}
