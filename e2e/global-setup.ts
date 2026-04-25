import { request } from '@playwright/test';
import * as fs from 'fs';
import * as path from 'path';

// Storage state file written by this setup and consumed by every test via
// `playwright.config.ts.use.storageState`. Kept absolute so it resolves
// correctly whether tests run from the repo root or e2e/.
const STORAGE_STATE = path.resolve(__dirname, '.auth-state.json');

export default async function globalSetup() {
  const baseURL = process.env.BASE_URL;
  if (!baseURL) {
    throw new Error('BASE_URL environment variable is required');
  }

  await waitForApp(baseURL);

  // OIDC env vars are optional — when unset, the backend runs in
  // auth-disabled mode and tests rely on the synthetic anonymous claim.
  // CI (docker-compose.test.yml) sets them; local-only runs may not.
  const tokenUrl = process.env.OIDC_TOKEN_URL;
  if (!tokenUrl) {
    console.log('OIDC_TOKEN_URL unset — skipping auth setup (auth-disabled mode)');
    // Empty storage state so playwright.config can reference the file
    // unconditionally without a "file not found" error.
    fs.writeFileSync(STORAGE_STATE, JSON.stringify({ cookies: [], origins: [] }));
    return;
  }

  const clientId = required('OIDC_CLIENT_ID');
  const clientSecret = required('OIDC_CLIENT_SECRET');
  const requiredScope = required('REQUIRED_SCOPE');

  await waitForMock(tokenUrl);

  const token = await fetchToken(tokenUrl, clientId, clientSecret, requiredScope);
  console.log(`Acquired test token (${token.length} chars) from ${tokenUrl}`);

  // Write a Playwright storage state file with the `auth` cookie. The cookie
  // domain MUST match the host the browser is hitting (app, in our compose
  // network) — Playwright otherwise drops cross-origin cookies on the floor.
  const baseHost = new URL(baseURL).hostname;
  const storage = {
    cookies: [
      {
        name: 'auth',
        value: token,
        domain: baseHost,
        path: '/',
        expires: -1,
        httpOnly: true,
        secure: true,
        sameSite: 'Lax' as const,
      },
    ],
    origins: [],
  };
  fs.writeFileSync(STORAGE_STATE, JSON.stringify(storage));

  // Expose the token to test files that exercise the API directly via
  // `request.newContext({ extraHTTPHeaders: { Authorization: ... } })`.
  process.env.AUTH_TOKEN = token;
}

function required(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`${name} env var is required when OIDC_TOKEN_URL is set`);
  return v;
}

async function waitForApp(baseURL: string): Promise<void> {
  const ctx = await request.newContext({ baseURL, ignoreHTTPSErrors: true });
  const maxAttempts = 60;
  const intervalMs = 500;
  for (let i = 0; i < maxAttempts; i++) {
    try {
      const res = await ctx.get('/health');
      if (res.ok()) {
        await ctx.dispose();
        console.log(`App ready at ${baseURL}`);
        return;
      }
    } catch {
      // not ready yet
    }
    await new Promise(r => setTimeout(r, intervalMs));
  }
  await ctx.dispose();
  throw new Error(`App at ${baseURL} did not become ready within ${(maxAttempts * intervalMs) / 1000}s`);
}

async function waitForMock(tokenUrl: string): Promise<void> {
  // Wait for the mock to come up by hitting the discovery endpoint, derived
  // from the token URL by stripping the trailing `/token` segment.
  const issuer = tokenUrl.replace(/\/token$/, '');
  const discoveryUrl = `${issuer}/.well-known/openid-configuration`;
  const ctx = await request.newContext({ ignoreHTTPSErrors: true });
  const maxAttempts = 60;
  const intervalMs = 500;
  for (let i = 0; i < maxAttempts; i++) {
    try {
      const res = await ctx.get(discoveryUrl);
      if (res.ok()) {
        await ctx.dispose();
        console.log(`Mock OIDC ready at ${issuer}`);
        return;
      }
    } catch {
      // not ready
    }
    await new Promise(r => setTimeout(r, intervalMs));
  }
  await ctx.dispose();
  throw new Error(`Mock OIDC at ${discoveryUrl} did not become ready`);
}

async function fetchToken(
  tokenUrl: string,
  clientId: string,
  clientSecret: string,
  scope: string,
): Promise<string> {
  const ctx = await request.newContext({ ignoreHTTPSErrors: true });
  try {
    const res = await ctx.post(tokenUrl, {
      form: {
        grant_type: 'client_credentials',
        client_id: clientId,
        client_secret: clientSecret,
        scope,
      },
    });
    if (!res.ok()) {
      throw new Error(`token request failed: ${res.status()} ${await res.text()}`);
    }
    // Read the body BEFORE disposing the context — Playwright frees the
    // response buffer on dispose, which would otherwise throw
    // "Response has been disposed".
    const body = await res.json();
    if (!body.access_token || typeof body.access_token !== 'string') {
      throw new Error(`token response missing access_token: ${JSON.stringify(body)}`);
    }
    return body.access_token;
  } finally {
    await ctx.dispose();
  }
}
