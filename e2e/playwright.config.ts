import { defineConfig, devices } from '@playwright/test';

const baseURL = process.env.BASE_URL;
if (!baseURL) {
  throw new Error('BASE_URL environment variable is required');
}

export default defineConfig({
  testDir: './tests',
  globalSetup: './global-setup',
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  use: {
    baseURL,
    headless: true,
    ignoreHTTPSErrors: true,
    screenshot: 'only-on-failure',
    trace: 'on-first-retry',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  reporter: [
    process.env.CI ? ['dot'] : ['list'],
    ['html', { open: 'never' }],
  ],
});
