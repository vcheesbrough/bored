import { defineConfig, devices } from '@playwright/test';
import * as path from 'path';

const baseURL = process.env.BASE_URL;
if (!baseURL) {
  throw new Error('BASE_URL environment variable is required');
}

// Storage state file produced by global-setup.ts. Always exists by the time
// tests run — even in auth-disabled mode it's a no-op `{cookies:[]}` file.
const STORAGE_STATE = path.resolve(__dirname, '.auth-state.json');

// `extraHTTPHeaders` apply to both `page.request` and Playwright's
// `request.newContext()`. global-setup writes AUTH_TOKEN before tests run,
// so request fixtures get the same bearer token as the browser does via
// the storage-state cookie.
const extraHTTPHeaders = process.env.AUTH_TOKEN
  ? { Authorization: `Bearer ${process.env.AUTH_TOKEN}` }
  : undefined;

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
    storageState: STORAGE_STATE,
    extraHTTPHeaders,
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
