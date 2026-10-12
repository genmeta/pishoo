/* @refresh reload */
import { render } from 'solid-js/web'

import App from './App.tsx'
import { I18nProvider } from './i18n'
import './index.css'
import './neo-brutal.css'

const root = document.getElementById('root')

if (!root) {
  throw new Error('missing #root element')
}

render(
  () => (
    <I18nProvider>
      <App />
    </I18nProvider>
  ),
  root,
)
