import { test, expect, request } from '@playwright/test';

// These tests assume the suite is run against the docker-compose.test.yml
// stack, which configures the backend with a mock OIDC provider and
// requires a valid bored:test:access scoped JWT on every /api/* call.
//
// `globalSetup` completes the authorization-code flow before tests run, so the
// happy-path browser contexts start with the backend's encrypted cookie set.

test.describe('auth — happy path', () => {
  test('browser session cookies are encrypted and hardened', async ({ page }) => {
    const cookies = await page.context().cookies();
    for (const name of ['auth', 'auth_refresh', 'auth_id']) {
      const cookie = cookies.find(candidate => candidate.name === name);
      expect(cookie, `${name} cookie`).toBeDefined();
      expect(cookie?.httpOnly).toBe(true);
      expect(cookie?.secure).toBe(true);
      expect(cookie?.sameSite).toBe('Lax');
      expect(cookie?.path).toBe('/');
      // JWTs have three dot-separated segments; private-cookie ciphertext does not.
      expect(cookie?.value.split('.')).toHaveLength(1);
    }
  });

  test('access token refreshes transparently near expiry', async ({ page }) => {
    const before = (await page.context().cookies()).find(cookie => cookie.name === 'auth_refresh');
    expect(before).toBeDefined();

    // The E2E issuer uses a 65-second access lifetime and production refreshes
    // inside 60 seconds. Crossing that boundary exercises the real grant.
    await page.waitForTimeout(6_000);
    const res = await page.request.get('/api/me');
    expect(res.status()).toBe(200);

    const after = (await page.context().cookies()).find(cookie => cookie.name === 'auth_refresh');
    expect(after).toBeDefined();
    expect(after?.value).not.toBe(before?.value);
  });

  test('GET /api/me returns the test service account identity', async ({ request }) => {
    const res = await request.get('/api/me');
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body).toHaveProperty('name');
    // The mock-oauth2-server returns the client_id as `sub` for
    // client_credentials tokens; preferred_username falls back to that.
    expect(body.name.length).toBeGreaterThan(0);
  });

  test('missing access cookie recovers through the refresh token', async ({ page }) => {
    await page.context().clearCookies({ name: /^auth$/ });
    const res = await page.request.get('/api/me');
    expect(res.status()).toBe(200);
    const restored = (await page.context().cookies()).find(cookie => cookie.name === 'auth');
    expect(restored).toBeDefined();
  });

  test('missing ID cookie does not prevent refresh recovery', async ({ page }) => {
    await page.context().clearCookies({ name: /^auth$/ });
    await page.context().clearCookies({ name: /^auth_id$/ });
    const res = await page.request.get('/api/me');
    expect(res.status()).toBe(200);
    const restored = (await page.context().cookies()).find(cookie => cookie.name === 'auth');
    expect(restored).toBeDefined();
  });

  test('bearer auth remains stateless and does not emit session cookies', async () => {
    const token = process.env.AUTH_TOKEN;
    expect(token).toBeTruthy();
    const ctx = await request.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: { Authorization: `Bearer ${token}` },
    });
    const res = await ctx.get('/api/me');
    expect(res.status()).toBe(200);
    expect(res.headers()['set-cookie']).toBeUndefined();
    await ctx.dispose();
  });

  test('tampered refresh cookie clears the browser session', async ({ page }) => {
    const cookies = await page.context().cookies();
    const refresh = cookies.find(cookie => cookie.name === 'auth_refresh');
    expect(refresh).toBeDefined();
    if (!refresh) return;

    await page.context().clearCookies({ name: /^auth$/ });
    await page.context().clearCookies({ name: /^auth_refresh$/ });
    await page.context().addCookies([
      {
        ...refresh,
        value: `${refresh.value.slice(0, -1)}${refresh.value.endsWith('A') ? 'B' : 'A'}`,
      },
    ]);

    const res = await page.request.get('/api/me');
    expect(res.status()).toBe(401);
    const remaining = await page.context().cookies();
    expect(remaining.filter(cookie => cookie.name.startsWith('auth')).map(cookie => cookie.name))
      .toEqual([]);
  });

  test('GET /auth/logout only shows a confirm form; POST clears tokens and forwards an ID token hint', async () => {
    // Logout invalidates the refresh-token chain, so this destructive test
    // must not consume the storage state shared by the rest of the suite.
    const ctx = await request.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: {},
    });
    const sessionCookies = async () =>
      (await ctx.storageState()).cookies
        .filter(cookie => cookie.name.startsWith('auth'))
        .map(cookie => cookie.name)
        .sort();

    try {
      const login = await ctx.get('/auth/login');
      expect(login.ok()).toBe(true);
      expect(await sessionCookies()).toEqual(['auth', 'auth_id', 'auth_refresh']);

      // Card #535: a GET (what an <img> or a third-party link sends) changes
      // nothing. It answers with a page whose one form POSTs the logout.
      const confirm = await ctx.get('/auth/logout', { maxRedirects: 0 });
      expect(confirm.status()).toBe(200);
      expect(confirm.headers()['set-cookie']).toBeUndefined();
      expect(await confirm.text()).toContain('<form method="post" action="/auth/logout">');
      expect(await sessionCookies()).toEqual(['auth', 'auth_id', 'auth_refresh']);
      expect((await ctx.get('/api/me')).status()).toBe(200);

      // The POST ends the session. A bare HTTP client sends neither Origin
      // nor Sec-Fetch-Site, which the server's cross-origin check lets by.
      const res = await ctx.post('/auth/logout', { maxRedirects: 0 });
      expect(res.status()).toBe(303);
      const location = new URL(res.headers()['location']);
      expect(location.searchParams.get('id_token_hint')).toBeTruthy();

      expect(await sessionCookies()).toEqual([]);
    } finally {
      await ctx.dispose();
    }
  });

  test('clicking "sign out" reaches the server and ends the session', async ({ browser }) => {
    // Card #252, then #535. The test above drives /auth/logout as bare HTTP
    // requests, which is why it never noticed that the old *link* did
    // nothing: leptos_router intercepts every same-origin <a> click and
    // routes it client-side. Only a real click in a real page exercises the
    // control. Since #535 the control is a form POST, and a real browser
    // sends it with Origin and Sec-Fetch-Site, so this test also proves the
    // server's cross-origin check accepts the SPA's own form.
    //
    // Own context, own login: logout revokes the refresh chain, so it must not
    // touch the storage state the rest of the suite shares.
    const ctx = await browser.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
    });
    try {
      const page = await ctx.newPage();
      // The mock IdP signs in without a form: /auth/login -> IdP -> callback
      // -> the SPA, which then renders the user badge from /api/me.
      await page.goto('/auth/login', { waitUntil: 'load' });
      const signOut = page.locator('form.user-logout-form button.user-logout');
      await expect(signOut).toBeVisible({ timeout: 15_000 });
      const sessionCookies = async () =>
        (await ctx.cookies()).filter(cookie => cookie.name.startsWith('auth')).map(c => c.name);
      expect(await sessionCookies()).toEqual(expect.arrayContaining(['auth', 'auth_refresh', 'auth_id']));

      // The click must leave the SPA as a top-level POST navigation that the
      // server answers with its redirect to the IdP's end-session endpoint.
      const logoutResponse = page.waitForResponse(
        res => new URL(res.url()).pathname === '/auth/logout',
        { timeout: 10_000 },
      );
      await signOut.click();
      const res = await logoutResponse;
      expect(res.request().method()).toBe('POST');
      expect(res.request().isNavigationRequest()).toBe(true);
      expect(res.status()).toBe(303);
      expect(new URL(res.headers()['location']).pathname).toContain('end-session');

      // Every session cookie is gone, so the next app load starts signed out.
      await expect.poll(sessionCookies, { timeout: 10_000 }).toEqual([]);
      // Liveness: the browser really left the SPA rather than staying on a
      // page that only looks signed out.
      await expect(page).not.toHaveURL(/\/auth\/logout$/);
    } finally {
      await ctx.close();
    }
  });

  test('a form on another site cannot sign the user out', async ({ browser }) => {
    // Card #535. SameSite=Lax keeps the session cookies off a cross-site
    // POST, but the browser still applies the *response's* Set-Cookie on a
    // top-level navigation, so without the server's cross-origin check the
    // "expire every session cookie" headers would sign the user out anyway.
    // This drives the real attack in a real browser: a page on another
    // origin submits a form to POST /auth/logout.
    const ctx = await browser.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
    });
    try {
      const page = await ctx.newPage();
      await page.goto('/auth/login', { waitUntil: 'load' });
      await expect(page.locator('form.user-logout-form button.user-logout'))
        .toBeVisible({ timeout: 15_000 });
      const sessionCookies = async () =>
        (await ctx.cookies())
          .filter(cookie => cookie.name.startsWith('auth'))
          .map(cookie => cookie.name)
          .sort();
      expect(await sessionCookies()).toEqual(['auth', 'auth_id', 'auth_refresh']);

      // The attacker's page, served by Playwright for a host that does not
      // exist: a genuinely different site from the app's.
      const appOrigin = new URL(process.env.BASE_URL!).origin;
      await page.route('https://attacker.invalid/**', route =>
        route.fulfill({
          contentType: 'text/html',
          body: `<!doctype html><form method="post" action="${appOrigin}/auth/logout">
                   <button id="go" type="submit">win a prize</button></form>`,
        }),
      );
      await page.goto('https://attacker.invalid/', { waitUntil: 'load' });

      const logoutResponse = page.waitForResponse(
        res => new URL(res.url()).pathname === '/auth/logout',
        { timeout: 10_000 },
      );
      await page.click('#go');
      const res = await logoutResponse;
      expect(res.request().method()).toBe('POST');
      expect(res.status()).toBe(403);

      // The session survived: every cookie is still there…
      expect(await sessionCookies()).toEqual(['auth', 'auth_id', 'auth_refresh']);
      // …and still works. Liveness: the app loads signed in again.
      expect((await ctx.request.get('/api/me')).status()).toBe(200);
      await page.goto('/', { waitUntil: 'load' });
      await expect(page.locator('form.user-logout-form button.user-logout'))
        .toBeVisible({ timeout: 15_000 });
    } finally {
      await ctx.close();
    }
  });

  test('GET /api/boards succeeds with the storage-state cookie', async ({ page }) => {
    // Do not use `networkidle`: the board opens an SSE connection that stays
    // active, so Playwright would wait until timeout.
    await page.goto('/', { waitUntil: 'load' });
    // The home page either shows the empty-state form or redirects to a
    // board view. Either is fine — the assertion is that the page does NOT
    // navigate to /auth/login (which would happen on an auth failure).
    await expect(page).not.toHaveURL(/\/auth\/login/, { timeout: 15_000 });
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
    // The shape the SPA depends on, asserted against the real backend rather
    // than a route mock. `env` is the deployment proper since card #412 — the
    // compose stack sets BORED__OBSERVABILITY__ENVIRONMENT=test — and `branch`
    // rides alongside it, absent here because no branch is configured. It is
    // this absence that makes the watermark render as a bare version.
    const info = await res.json();
    expect(info.env).toBe('test');
    expect(info.branch ?? null).toBeNull();
    expect(typeof info.version).toBe('string');
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
    expect(new URL(location).searchParams.get('scope')).toContain('offline_access');
    // The telemetry ingest refuses a bearer without it (card #416).
    expect(new URL(location).searchParams.get('scope')?.split(' ')).toContain('telemetry:write');
    await ctx.dispose();
  });

  test('GET /auth/login binds the return path into OAuth state', async () => {
    const ctx = await request.newContext({
      ...unauthOptions,
      maxRedirects: 0,
    });
    const returnTo = '/boards/old-board?card=42';
    const res = await ctx.get(`/auth/login?return_to=${encodeURIComponent(returnTo)}`);
    const location = new URL(res.headers()['location']);
    const state = location.searchParams.get('state') ?? '';
    const encodedReturnTo = state.split('.')[1] ?? '';

    expect(Buffer.from(encodedReturnTo, 'base64url').toString()).toBe(returnTo);
    await ctx.dispose();
  });
});
