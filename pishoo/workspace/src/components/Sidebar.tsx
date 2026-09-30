import { For, Show } from 'solid-js'

import type { MessageKey } from '../i18n'
import { useI18n } from '../i18n'
import type { WorkspacePage } from '../lib/routes'
import { Avatar } from './Ui'

type NavItem = {
  page: WorkspacePage
  href: string
  label: MessageKey
  mobileLabel?: MessageKey
}

const PRIMARY_NAV: NavItem[] = [
  { page: 'contacts', href: '/workspace/contacts', label: 'nav.contacts', mobileLabel: 'nav.mobileContacts' },
  { page: 'extensions', href: '/workspace/extensions', label: 'nav.extensions', mobileLabel: 'nav.mobileExtensions' },
  { page: 'apps', href: '/workspace/apps', label: 'nav.apps', mobileLabel: 'nav.mobileApps' },
  { page: 'approvals', href: '/workspace/approvals', label: 'nav.reviews', mobileLabel: 'nav.mobileReviews' },
  { page: 'profile', href: '/workspace/settings/profile', label: 'nav.settings', mobileLabel: 'nav.mobileSettings' },
]

export interface SidebarProps {
  activePage: WorkspacePage
  profileName: string
  profileIdentity?: string
  profileAvatar: string | null
  pendingCount: number
  follow: (event: MouseEvent, href: string) => void
}

function navAccessibleName(
  item: NavItem,
  t: ReturnType<typeof useI18n>['t'],
  pendingCount: number,
): string | undefined {
  if (!item.mobileLabel) return undefined
  if (item.page === 'approvals' && pendingCount > 0) {
    return t(item.label) + ', ' + t('nav.pendingReviews', { count: pendingCount })
  }
  return t(item.label)
}

export default function Sidebar(props: SidebarProps) {
  const { locale, setLocale, t } = useI18n()

  return (
    <aside class="sidebar">
      <a class="brand" href="/workspace/" onClick={(event) => props.follow(event, '/workspace/')}>
        <span class="brand-mark" aria-hidden="true">P</span>
        <span><strong>pishoo</strong><small>{t('brand.subtitle')}</small></span>
      </a>
      <a class="profile-summary" href="/workspace/" onClick={(event) => props.follow(event, '/workspace/')}>
        <Avatar name={props.profileName} src={props.profileAvatar} />
        <span>
          <strong>{props.profileName}</strong>
          <Show when={props.profileIdentity && props.profileIdentity !== props.profileName}>
            <small>{props.profileIdentity}</small>
          </Show>
        </span>
      </a>
      <nav class="primary-nav" aria-label={t('nav.primary')}>
        <For each={PRIMARY_NAV}>{(item) => {
          const active = () => props.activePage === item.page
          return (
            <a href={item.href} classList={{ active: active() }} aria-current={active() ? 'page' : undefined}
              aria-label={navAccessibleName(item, t, props.pendingCount)}
              onClick={(event) => props.follow(event, item.href)}>
              <span class="nav-label-full">{t(item.label)}</span>
              <Show when={item.mobileLabel} keyed>
                {(label) => <span class="nav-label-mobile" aria-hidden="true">{t(label)}</span>}
              </Show>
              <Show when={item.page === 'approvals' && props.pendingCount > 0}>
                <span class="nav-count" aria-label={t('nav.pendingReviews', { count: props.pendingCount })}>
                  {props.pendingCount}
                </span>
              </Show>
            </a>
          )
        }}</For>
      </nav>
      <div class="language-switch" role="group" aria-label={t('language.label')}>
        <button type="button" aria-label={t('language.english')} aria-pressed={locale() === 'en'}
          classList={{ active: locale() === 'en' }} onClick={() => setLocale('en')}>EN</button>
        <button type="button" aria-label={t('language.chinese')} aria-pressed={locale() === 'zh-CN'}
          classList={{ active: locale() === 'zh-CN' }} onClick={() => setLocale('zh-CN')}>中文</button>
      </div>
    </aside>
  )
}
