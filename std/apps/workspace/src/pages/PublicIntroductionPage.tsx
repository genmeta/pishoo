import { Show, createEffect, createResource, createSignal, onCleanup, onMount } from 'solid-js'

import { api } from '../api/client'
import { Avatar, ErrorState, LoadingState } from '../components/Ui'
import { useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { displayIdentityName, errorMessage, normalizeIdentityName, profileAvatarUrl } from '../lib/format'
import { addContactUrl } from '../lib/routes'

export default function PublicIntroductionPage() {
  const { locale, setLocale, t } = useI18n()
  const load = abortable((_source: boolean, signal: AbortSignal) => api.introduction(signal))
  const [introduction, actions] = createResource(() => true, load)
  const [choosingIdentity, setChoosingIdentity] = createSignal(false)
  const [viewerName, setViewerName] = createSignal('')
  const [identityError, setIdentityError] = createSignal<string | null>(null)
  let identityInput: HTMLInputElement | undefined
  let card: HTMLDivElement | undefined
  let cardBounds: DOMRect | undefined
  let tiltPreference: MediaQueryList | undefined
  let tiltFrame = 0
  let pointerX = 0
  let pointerY = 0

  const details = () => introduction.error ? undefined : introduction()
  const identityName = () => details()?.identity_name ?? ''
  const displayName = () => details()?.display_name || displayIdentityName(identityName())
  const isOwnProfile = () => details()?.is_owner ?? false
  const contactUrl = () => addContactUrl(details()?.viewer_name ?? '', identityName())

  const resetTilt = () => {
    cancelAnimationFrame(tiltFrame)
    tiltFrame = 0
    cardBounds = undefined
    card?.style.removeProperty('--intro-rotate-x')
    card?.style.removeProperty('--intro-rotate-y')
    card?.style.removeProperty('--intro-shine-offset')
  }

  onMount(() => {
    tiltPreference = window.matchMedia('(hover: hover) and (pointer: fine) and (prefers-reduced-motion: no-preference)')
    tiltPreference.addEventListener('change', resetTilt)
  })

  onCleanup(() => {
    tiltPreference?.removeEventListener('change', resetTilt)
    resetTilt()
  })

  const moveCard = (event: PointerEvent) => {
    if (!tiltPreference?.matches || event.pointerType !== 'mouse' || !cardBounds) return
    pointerX = event.clientX
    pointerY = event.clientY
    if (tiltFrame) return
    // Coalesce pointer events; no animation work runs while the card is idle.
    tiltFrame = requestAnimationFrame(() => {
      tiltFrame = 0
      if (!card || !cardBounds) return
      const x = Math.max(-1, Math.min(1, (pointerX - cardBounds.left) / cardBounds.width * 2 - 1))
      const y = Math.max(-1, Math.min(1, (pointerY - cardBounds.top) / cardBounds.height * 2 - 1))
      card.style.setProperty('--intro-rotate-x', `${-y * 4}deg`)
      card.style.setProperty('--intro-rotate-y', `${x * 5}deg`)
      card.style.setProperty('--intro-shine-offset', `${x * 28}px`)
    })
  }

  createEffect(() => {
    document.title = `${displayName() || t('intro.title')} · pishoo`
  })

  const chooseIdentity = () => {
    setChoosingIdentity(true)
    queueMicrotask(() => identityInput?.focus())
  }

  const continueToWorkspace = () => {
    const viewer = normalizeIdentityName(viewerName())
    if (!viewer) {
      setIdentityError(t('intro.invalidIdentity'))
    } else if (viewer === normalizeIdentityName(identityName())) {
      setIdentityError(t('outbound.ownIdentity'))
    } else {
      const url = addContactUrl(viewer, identityName())
      if (url) window.open(url, '_blank', 'noopener')
      return
    }
    identityInput?.focus()
  }

  return (
    <div class="public-introduction">
      <header class="introduction-header">
        <div class="language-switch" role="group" aria-label={t('language.label')}>
          <button type="button" aria-label={t('language.english')} aria-pressed={locale() === 'en'}
            classList={{ active: locale() === 'en' }} onClick={() => setLocale('en')}>EN</button>
          <button type="button" aria-label={t('language.chinese')} aria-pressed={locale() === 'zh-CN'}
            classList={{ active: locale() === 'zh-CN' }} onClick={() => setLocale('zh-CN')}>中文</button>
        </div>
      </header>
      <main class="introduction-main">
        <section class="introduction-content" aria-label={t('intro.title')}>
          <Show when={introduction.loading && !details()}><LoadingState /></Show>
          <Show when={introduction.error}>
            <ErrorState message={errorMessage(introduction.error, t('common.unexpectedError'))}
              onRetry={() => void actions.refetch()} />
          </Show>
          <Show when={details()}>
            <div class="introduction-card-stage"
              onPointerEnter={(event) => { cardBounds = event.currentTarget.getBoundingClientRect() }}
              onPointerMove={moveCard} onPointerLeave={resetTilt} onPointerCancel={resetTilt}>
              <div class="introduction-card" ref={card}>
                <div class="introduction-card-art" aria-hidden="true" />
                <div class="introduction-card-top">
                  <span class="introduction-card-brand">pishoo</span>
                </div>
                <div class="introduction-person">
                  <Avatar name={displayName()} src={profileAvatarUrl(details())} large />
                  <div class="introduction-person-name">
                    <h1>{displayName()}</h1>
                    <div class="introduction-handle">
                      <span aria-hidden="true">@</span>
                      <p class="introduction-identity"><span class="sr-only">{t('profile.identity')}: </span>
                        {displayIdentityName(identityName())}
                      </p>
                    </div>
                  </div>
                </div>
                <div class="introduction-card-bottom">
                  <span>{t('intro.cardLabel')}</span>
                </div>
              </div>
            </div>
            <div class="introduction-connect">
              <h2>{t('intro.connectTitle')}</h2>
              <div class="introduction-actions">
                <Show when={!isOwnProfile()} fallback={
                  <>
                    <p class="muted">{t('intro.ownProfile')}</p>
                    <a class="button button-primary" href="/workspace/settings/profile">{t('intro.editProfile')}</a>
                  </>
                }>
                  <Show when={contactUrl()} fallback={
                    <button class="button button-primary" type="button" onClick={chooseIdentity}
                      aria-expanded={choosingIdentity()} aria-controls="introduction-identity-form">
                      {t('intro.addFriend')}
                    </button>
                  }>
                    <a class="button button-primary" href={contactUrl()!} target="_blank" rel="noopener">
                      <span class="introduction-action-label">{t('intro.addAs', {
                        name: displayIdentityName(details()?.viewer_name ?? ''),
                      })}</span>
                      <span aria-hidden="true">↗</span>
                    </a>
                  </Show>
                </Show>
              </div>
              <Show when={choosingIdentity()}>
                <form id="introduction-identity-form" class="introduction-identity-form"
                  onSubmit={(event) => { event.preventDefault(); continueToWorkspace() }}>
                  <label class="field"><span>{t('intro.yourIdentity')}</span>
                    <input ref={identityInput} value={viewerName()} autocomplete="off" autocapitalize="none"
                      spellcheck={false} aria-required="true" aria-invalid={identityError() !== null}
                      aria-describedby={identityError() ? 'introduction-identity-error' : undefined}
                      onInput={(event) => { setViewerName(event.currentTarget.value); setIdentityError(null) }} />
                  </label>
                  <Show when={identityError()}>
                    <p class="field-error" id="introduction-identity-error" role="alert">{identityError()}</p>
                  </Show>
                  <button class="button button-secondary" type="submit">{t('intro.continue')}</button>
                </form>
              </Show>
            </div>
          </Show>
        </section>
      </main>
      <footer class="introduction-footer"><span>pishoo</span><span>{t('intro.footer')}</span></footer>
    </div>
  )
}
