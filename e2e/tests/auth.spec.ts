import { test, expect, request } from '@playwright/test';

// These tests assume the suite is run against the docker-compose.test.yml
// stack, which configures the backend with a mock OIDC provider and
// requires a valid bored:test:access scoped JWT on every /api/* call.
//
// `globalSetup` writes the cookie + bearer token before tests run, so the
// "happy path" tests below execute as the authenticated test user.

test.describe('auth — happy path', () => {
  test('GET /api/me returns the test service account identity', async ({ request }) => {
    const res = await request.get('/api/me');
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body).toHaveProperty('name');
    // The mock-oauth2-server returns the client_id as `sub` for
    // client_credentials tokens; preferred_username falls back to that.
    expect(body.name.length).toBeGreaterThan(0);
  });

  test('GET /api/boards succeeds with the storage-state cookie', async ({ page }) => {
    await page.goto('/');
    // The home page either shows the empty-state form or redirects to a
    // board view. Either is fine — the assertion is that the page does NOT
    // navigate to /auth/login (which would happen on an auth failure).
    await page.waitForLoadState('networkidle');
    expect(page.url()).not.toContain('/auth/login');
  });
});

test.describe('auth — rejection', () => {
  // Playwright's `request.newContext()` (imported from @playwright/test)
  // inherits the project's `use.storageState` and `use.extraHTTPHeaders` —
  // so to create a truly unauthenticated context we have to explicitly
  // override BOTH to empty values. Without these overrides our test would
  // silently piggy-back on the global auth cookie.
  const unauthOptions = {
    baseURL: process.env.BASE_URL,
    ignoreHTTPSErrors: true,
    storageState: { cookies: [], origins: [] },
    extraHTTPHeaders: {},
  };

  test('GET /api/boards without a token returns 401', async () => {
    const ctx = await request.newContext(unauthOptions);
    const res = await ctx.get('/api/boards');
    expect(res.status()).toBe(401);
    await ctx.dispose();
  });

  test('GET /api/me with a malformed bearer token returns 401', async () => {
    const ctx = await request.newContext({
      ...unauthOptions,
      extraHTTPHeaders: {
        Authorization: 'Bearer not.a.real.jwt',
      },
    });
    const res = await ctx.get('/api/me');
    expect(res.status()).toBe(401);
    await ctx.dispose();
  });
});

test.describe('auth — public routes', () => {
  // Same caveat as above: explicit empty storageState/headers so that the
  // "this is reachable WITHOUT a token" tests prove what they claim to.
  const unauthOptions = {
    baseURL: process.env.BASE_URL,
    ignoreHTTPSErrors: true,
    storageState: { cookies: [], origins: [] },
    extraHTTPHeaders: {},
  };

  test('GET /health is reachable without a token', async () => {
    const ctx = await request.newContext(unauthOptions);
    const res = await ctx.get('/health');
    expect(res.status()).toBe(200);
    await ctx.dispose();
  });

  test('GET /api/info is public', async () => {
    const ctx = await request.newContext(unauthOptions);
    const res = await ctx.get('/api/info');
    expect(res.status()).toBe(200);
    await ctx.dispose();
  });

  test('GET /auth/login redirects to the IdP', async () => {
    const ctx = await request.newContext({
      ...unauthOptions,
      // Don't follow redirects automatically so we can assert on the 3xx.
      maxRedirects: 0,
    });
    const res = await ctx.get('/auth/login');
    // 302 or 307 depending on axum version; either is a valid redirect.
    expect([301, 302, 303, 307]).toContain(res.status());
    const location = res.headers()['location'] ?? '';
    expect(location).toContain('mock-oidc');
    expect(location).toContain('client_id=');
    expect(location).toContain('state=');
    await ctx.dispose();
  });
});
