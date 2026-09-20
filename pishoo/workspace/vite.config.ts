import { defineConfig } from 'vite'
import solid from 'vite-plugin-solid'

const backend = process.env.PISHOO_WORKSPACE_BACKEND ?? 'http://127.0.0.1:3000'

export default defineConfig({
  base: '/workspace/',
  plugins: [solid()],
  server: {
    proxy: {
      '/workspace-api': backend,
      '/acl': backend,
      '/contact': backend,
      '/contacts': backend,
    },
  },
})
