import { defineConfig } from 'vite'
import solid from 'vite-plugin-solid'

const backend = process.env.PISHOO_WORKSPACE_BACKEND ?? 'http://127.0.0.1:3000'

export default defineConfig({
  base: '/std/workspace/',
  plugins: [solid()],
  server: {
    proxy: {
      '/std/workspace-api': backend,
      '/std/chat-api': backend,
      '/std/profile': backend,
      '/std/api': backend,
      '/std/acl': backend,
      '/std/contact': backend,
      '/std/contacts': backend,
    },
  },
})
