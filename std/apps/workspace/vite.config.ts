import { defineConfig } from 'vite'
import solid from 'vite-plugin-solid'

const backend = process.env.PISHOO_WORKSPACE_BACKEND ?? 'http://127.0.0.1:3000'

export default defineConfig({
  base: './',
  plugins: [solid()],
  server: {
    proxy: {
      // App and Lib share /std/workspace; proxy only declared business routes.
      '^/std/workspace/(context|settings|profiles|contact-requests|contacts|contact-directory|approvals|intro/context)(/|\\?|$)': backend,
      '/std/profile': backend,
      '/std/message': backend,
      '/std/sys/package': backend,
      '/std/pishoo': backend,
      '/std/dhttp': backend,
      '/std/acl': backend,
      '/std/contact': backend,
      '/std/contacts': backend,
    },
  },
})
