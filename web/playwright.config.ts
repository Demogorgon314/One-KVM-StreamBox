import { defineConfig } from '@playwright/test'

const targetBaseUrl = process.env.ONE_KVM_E2E_TARGET_URL || 'http://192.168.12.156'

export default defineConfig({
  testDir: './tests',
  timeout: 90_000,
  expect: {
    timeout: 10_000,
  },
  retries: 1,
  use: {
    baseURL: targetBaseUrl,
    headless: true,
    ignoreHTTPSErrors: true,
    trace: 'retain-on-failure',
  },
  projects: [
    {
      name: 'desktop-1366',
      use: {
        viewport: { width: 1366, height: 768 },
      },
    },
  ],
})
