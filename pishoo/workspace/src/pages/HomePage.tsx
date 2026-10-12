import { Show } from 'solid-js'

import { Avatar } from '../components/Ui'
import { useI18n } from '../i18n'
import { displayIdentityName } from '../lib/format'

export default function HomePage(props: {
  ownerName: string
  displayName: string | null
  avatarUrl: string | null
  pendingCount: number
  pendingLoading: boolean
  pendingError: unknown
  follow: (event: MouseEvent, href: string) => void
}) {
  const { t } = useI18n()
  const identityName = () => displayIdentityName(props.ownerName)
  return (
    <>
      <header class="page-header"><h1>{t('home.title')}</h1></header>
      <div class="workspace-overview">
        <section class="overview-panel">
          <h2>{t('home.identity')}</h2>
          <div class="overview-identity">
            <Avatar name={props.displayName || identityName()} src={props.avatarUrl} large />
            <div class="overview-identity-details">
              <p class="overview-name">{props.displayName || identityName()}</p>
              <Show when={props.displayName && props.displayName !== identityName()}>
                <p class="overview-identity-name">
                  <code>{identityName()}</code>
                </p>
              </Show>
            </div>
          </div>
        </section>
        <section class="overview-panel">
          <h2>{t('home.todo')}</h2>
          <Show when={!props.pendingLoading && !props.pendingError} fallback={
            <p class="muted">{t(props.pendingLoading ? 'common.loading' : 'home.todoUnavailable')}</p>
          }>
            <p>{t('home.pendingReviews', { count: props.pendingCount })}</p>
          </Show>
          <a class="button button-primary overview-action" href="/std/workspace/approvals"
            onClick={(event) => props.follow(event, '/std/workspace/approvals')}>
            {t('home.goReviews')}
          </a>
        </section>
      </div>
    </>
  )
}
