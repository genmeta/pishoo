import { defineConfig } from '@playwright/test'

const backendUrl = process.env.PISHOO_WORKSPACE_E2E_BACKEND
const frontendUrl = 'http://127.0.0.1:4173'

if (!backendUrl) {
  throw new Error(
    'PISHOO_WORKSPACE_E2E_BACKEND must point to an isolated pishoo profile backend',
  )
}

export default defineConfig({
  testDir: './e2e',
  fullyParallel: false,
  workers: 1,
  timeout: 30_000,
  expect: { timeout: 8_000 },
  reporter: [['list'], ['html', { open: 'never' }]],
  use: {
    baseURL: `${frontendUrl}/std/workspace/`,
    locale: 'en-US',
    trace: 'retain-on-failure',
  },
  webServer: [
    {
      command: 'bun run dev -- --host 127.0.0.1 --port 4173 --strictPort',
      env: { PISHOO_WORKSPACE_BACKEND: backendUrl },
      url: `${frontendUrl}/std/workspace/`,
      timeout: 30_000,
      reuseExistingServer: false,
      stdout: 'pipe',
      stderr: 'pipe',
    },
  ],
  projects: [
    {
      name: 'desktop',
      use: { browserName: 'chromium', viewport: { width: 1440, height: 900 } },
    },
    {
      name: 'tablet',
      use: { browserName: 'chromium', viewport: { width: 768, height: 1024 } },
    },
    {
      name: 'mobile',
      use: {
        browserName: 'chromium',
        viewport: { width: 375, height: 812 },
        hasTouch: true,
        isMobile: true,
      },
    },
  ],
})
