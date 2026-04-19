import { request } from '@playwright/test';

export default async function globalSetup() {
  const baseURL = process.env.BASE_URL;
  if (!baseURL) {
    throw new Error('BASE_URL environment variable is required');
  }

  const maxAttempts = 60;
  const intervalMs = 500;

  for (let i = 0; i < maxAttempts; i++) {
    try {
      const ctx = await request.newContext({ baseURL, ignoreHTTPSErrors: true });
      const res = await ctx.get('/health');
      await ctx.dispose();
      if (res.ok()) {
        console.log(`\nApp ready at ${baseURL}`);
        return;
      }
    } catch {
      // not ready yet
    }
    await new Promise(r => setTimeout(r, intervalMs));
  }

  throw new Error(
    `App at ${baseURL} did not become ready within ${(maxAttempts * intervalMs) / 1000}s`
  );
}
