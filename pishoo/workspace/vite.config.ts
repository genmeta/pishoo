import { defineConfig } from 'vite'
import solid from 'vite-plugin-solid'

const backend = process.env.PISHOO_WORKSPACE_BACKEND ?? 'http://127.0.0.1:3000'

export default defineConfig({
  base: './',
  plugins: [solid()],
  server: {
    proxy: {
      '/api': backend,
      '/std/pishoo': backend,
      '/std/dhttp': backend,
      '/std/acl': backend,
      '/std/contact': backend,
      '/std/contacts': backend,
    },
  },
})
