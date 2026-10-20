import { createSignal } from 'solid-js'

import { useI18n } from '../i18n'

export default function IntroductionShare() {
  const { t } = useI18n()
  const url = new URL('/workspace/intro', window.location.origin).href
  const [message, setMessage] = createSignal('')
  let linkInput: HTMLInputElement | undefined

  const copyLink = async () => {
    try {
      await navigator.clipboard.writeText(url)
      setMessage(t('intro.linkCopied'))
    } catch {
      linkInput?.focus()
      linkInput?.select()
      setMessage(t('intro.copyManually'))
    }
  }

  return (
    <section class="settings-panel introduction-share" aria-label={t('intro.shareTitle')}>
      <h2>{t('intro.shareTitle')}</h2>
      <label class="field"><span>{t('intro.shareLink')}</span>
        <input ref={linkInput} value={url} readonly onFocus={(event) => event.currentTarget.select()} />
      </label>
      <div class="introduction-share-actions">
        <a class="button button-secondary" href={url} target="_blank" rel="noopener">{t('intro.preview')}</a>
        <button class="button button-primary" type="button" onClick={() => void copyLink()}>{t('intro.copyLink')}</button>
      </div>
      <p class="muted" role="status">{message()}</p>
    </section>
  )
}
