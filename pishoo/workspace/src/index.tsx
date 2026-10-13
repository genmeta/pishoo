/* @refresh reload */
import { Show, createSignal, onCleanup } from 'solid-js'
import { render } from 'solid-js/web'

import App from './App.tsx'
import { I18nProvider } from './i18n'
import PublicIntroductionPage from './pages/PublicIntroductionPage'
import './index.css'
import './neo-brutal.css'

const root = document.getElementById('root')

if (!root) {
  throw new Error('missing #root element')
}

render(
  () => {
    const [hash, setHash] = createSignal(window.location.hash)
    const update = () => setHash(window.location.hash)
    window.addEventListener('hashchange', update)
    window.addEventListener('popstate', update)
    onCleanup(() => {
      window.removeEventListener('hashchange', update)
      window.removeEventListener('popstate', update)
    })
    return (
    <I18nProvider>
      <Show when={hash().split('?')[0].replace(/\/$/, '') === '#/intro'} fallback={<App />}><PublicIntroductionPage /></Show>
    </I18nProvider>
    )
  },
  root,
)
