import { expect, test as base, type Page } from '@playwright/test';
import * as fs from 'fs';
import { apiCreateBoard, apiCreateCard, apiCreateColumn, gotoBoardView, openChooser } from './helpers';

// Card #416: the SPA's own traces and logs, sent over OTLP to the
// environment's ingest (`otlp-collector-oidc`), which authenticates the
// session's bearer, stamps identity and forwards to a collector.
//
// In this rig the SPA reaches the ingest at https://collector:4318, cross-origin
// with CORS allowed for https://app (production serves it same-origin, through
// Traefik), and the ingest forwards to a fake receiver — a stock OpenTelemetry
// Collector with a file exporter — whose JSON-lines output is mounted
// read-only here. Assertions are on what that receiver actually got, never on
// configuration.

/** The ingest's base URL, as `/api/info` hands it to the SPA in this rig. */
const INGEST = 'https://collector:4318';

/**
 * Every test here gets a page with a session of its own, signed in through the
 * real `/auth/login` flow, rather than the suite's shared `storageState`.
 *
 * The shared state is one refresh token that every other spec's page starts
 * from, and the backend's refresh cache hands the same rotation to every
 * request presenting the same old token for up to two minutes. With the mock's
 * 65-second access tokens, a page following that shared chain can be handed an
 * access token that has already expired by the ingest's reckoning — which the
 * token route rightly refuses (`503`, retry). Production browsers each have
 * their own chain and 15-minute tokens; a fresh sign-in per test gives these
 * tests the same.
 */
const test = base.extend({
  page: async ({ browser, baseURL }, use) => {
    const context = await browser.newContext({
      baseURL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
    });
    const page = await context.newPage();
    // Land on `/health` — plain text, no SPA — so the test's own first
    // navigation does not abort a half-loaded app (which would surface as a
    // page error in the liveness checks).
    await page.goto('/auth/login?return_to=/health');
    await page.waitForURL((url) => url.pathname === '/health');
    await use(page);
    await context.close();
  },
});

/** Where the fake receiver writes what reached it (one export per line). */
const OTLP_OUT = process.env.OTLP_OUT ?? '/otlp/otlp.jsonl';

type Attr = { key: string; value: Record<string, unknown> };
type Resource = { attributes?: Attr[] };
type ExportedSpan = {
  traceId: string;
  spanId: string;
  parentSpanId?: string;
  name: string;
  startTimeUnixNano: string;
  attributes?: Attr[];
  links?: { traceId: string; spanId: string }[];
  resource: Resource;
};
type ExportedLog = {
  timeUnixNano: string;
  body?: { stringValue?: string };
  attributes?: Attr[];
  traceId?: string;
  spanId?: string;
  resource: Resource;
};

/** Everything the receiver has written so far, flattened. */
function readReceiver(): { spans: ExportedSpan[]; logs: ExportedLog[] } {
  const spans: ExportedSpan[] = [];
  const logs: ExportedLog[] = [];
  if (!fs.existsSync(OTLP_OUT)) return { spans, logs };
  for (const line of fs.readFileSync(OTLP_OUT, 'utf8').split('\n')) {
    if (!line.trim()) continue;
    // The file exporter can be mid-write on the last line; skip what does
    // not parse yet — the next poll sees it whole.
    let parsed: any;
    try {
      parsed = JSON.parse(line);
    } catch {
      continue;
    }
    for (const rs of parsed.resourceSpans ?? []) {
      for (const ss of rs.scopeSpans ?? []) {
        for (const span of ss.spans ?? []) spans.push({ ...span, resource: rs.resource ?? {} });
      }
    }
    for (const rl of parsed.resourceLogs ?? []) {
      for (const sl of rl.scopeLogs ?? []) {
        for (const log of sl.logRecords ?? []) logs.push({ ...log, resource: rl.resource ?? {} });
      }
    }
  }
  return { spans, logs };
}

function attr(attrs: Attr[] | undefined, key: string): unknown {
  const found = attrs?.find((a) => a.key === key);
  if (!found) return undefined;
  const v = found.value as Record<string, unknown>;
  return v.stringValue ?? v.intValue ?? v.boolValue;
}

const service = (r: Resource) => attr(r.attributes, 'service.name');

// Never pick "this test's span" by comparing its timestamp with the test
// runner's `Date.now()` (card #454). The SPA stamps spans with
// `performance.timeOrigin + performance.now()`, and in headless Chromium that
// clock can lag the system wall clock by over a second for a whole document —
// measured at -1.09 s to -1.19 s in exactly the runs that failed, and ~0 in
// every run that passed. A span that did arrive then looked older than the
// test's mark and was never found. Spans are identified by what they did
// instead (the board they loaded), or against a mark read from the page's own
// clock.

/**
 * The `screen board` span of a load of `boardName`, found through the request
 * that load made: the server's span for `GET /api/boards/<name>/columns` (the
 * name is unique per test) → its parent, the browser's `http.client` span →
 * its parent, the screen span.
 */
function boardLoad(boardName: string): ExportedSpan | undefined {
  const { spans } = readReceiver();
  const byId = new Map(spans.map((s) => [s.spanId, s]));
  for (const server of spans) {
    if (
      service(server.resource) !== 'bored' ||
      attr(server.attributes, 'http.request.method') !== 'GET' ||
      attr(server.attributes, 'url.path') !== `/api/boards/${boardName}/columns` ||
      !server.parentSpanId
    ) {
      continue;
    }
    const request = byId.get(server.parentSpanId);
    const screen = request?.parentSpanId ? byId.get(request.parentSpanId) : undefined;
    if (screen?.name === 'screen board' && service(screen.resource) === 'bored-spa') return screen;
  }
  return undefined;
}

/**
 * The page's telemetry clock, read in the page: the same
 * `performance.timeOrigin + performance.now()` the SPA stamps spans with, so a
 * span started after this call carries a start time at or after it whatever
 * that clock's offset from the runner's.
 */
function pageClockMs(page: Page): Promise<number> {
  return page.evaluate(() => performance.timeOrigin + performance.now());
}

/**
 * Whether an OTLP nanosecond timestamp is at or after a page-clock mark. The
 * 1 ms allowance absorbs float rounding between the two conversions to
 * nanoseconds; anything from an earlier test is seconds older.
 */
const atOrAfter = (nanos: string, markMs: number) => Number(BigInt(nanos) / 1_000n) / 1_000 >= markMs - 1;

/** Poll the receiver until `pick` finds something, or fail after `timeout`. */
async function waitFor<T>(pick: () => T | undefined, what: string, timeout = 30_000): Promise<T> {
  let found: T | undefined;
  await expect
    .poll(
      () => {
        found = pick();
        return found !== undefined;
      },
      { message: `waiting for ${what}`, timeout, intervals: [500] },
    )
    .toBe(true);
  return found as T;
}

/** Console lines the exporter writes (every one starts `telemetry:`). */
function watchTelemetryConsole(page: Page) {
  const lines: string[] = [];
  page.on('console', (msg) => {
    if (msg.text().startsWith('telemetry:')) lines.push(msg.text());
  });
  return lines;
}

/** Liveness: no panic, no page error, and the board still reacts. */
function watchCrashes(page: Page) {
  const crashes: string[] = [];
  page.on('pageerror', (err) => crashes.push(err.message));
  page.on('console', (msg) => {
    if (/wasm panic|already been disposed/.test(msg.text())) crashes.push(msg.text());
  });
  return crashes;
}

async function boardWithCard(request: import('@playwright/test').APIRequestContext, prefix: string) {
  const board = await apiCreateBoard(request, `${prefix}-${Date.now()}`);
  const column = await apiCreateColumn(request, board.name, 'Column');
  await apiCreateCard(request, column.id, 'Telemetry card');
  return board;
}

test.describe('client telemetry', () => {
  test('a board load is one trace: screen span → http.client span → server span', async ({ page, request }) => {
    const board = await boardWithCard(request, 'otel-trace');
    await gotoBoardView(page, board.name);

    // The screen-load span: a browser root. Found through the chain this test
    // is about (server span → http.client span → screen span), so its being
    // found already proves the parent links; the assertions below check the
    // attributes along it.
    const screen = await waitFor(() => boardLoad(board.name), 'the screen board span');
    expect(screen.parentSpanId ?? '').toBe('');
    expect(attr(screen.attributes, 'bored.screen')).toBe('board');

    // Its child: the columns request, carrying semconv attributes.
    const columns = await waitFor(
      () =>
        readReceiver().spans.find(
          (s) => s.traceId === screen.traceId && s.name === 'GET /api/boards/{slug}/columns',
        ),
      'the columns http.client span',
    );
    expect(columns.parentSpanId).toBe(screen.spanId);
    expect(attr(columns.attributes, 'http.request.method')).toBe('GET');
    expect(attr(columns.attributes, 'url.template')).toBe('/api/boards/{slug}/columns');
    expect(String(attr(columns.attributes, 'http.response.status_code'))).toBe('200');

    // Identity and environment were stamped by the ingest, not sent by us —
    // and the narrowed CLAIM_ATTRIBUTES (decision D2) keeps email out.
    expect(attr(columns.attributes, 'user.name')).toBe('test-user');
    expect(attr(columns.attributes, 'user.id')).toBeTruthy();
    expect(attr(columns.attributes, 'user.email')).toBeUndefined();
    expect(attr(columns.resource.attributes, 'deployment.environment.name')).toBe('test');
    expect(attr(columns.resource.attributes, 'telemetry_source')).toBe('client');

    // The server's span for that request joined the same trace, as a child of
    // the browser's span: `traceparent` crossed the edge and was adopted.
    const server = await waitFor(
      () =>
        readReceiver().spans.find(
          (s) => service(s.resource) === 'bored' && s.parentSpanId === columns.spanId,
        ),
      "the server's span under the browser request",
    );
    expect(server.traceId).toBe(screen.traceId);
  });

  test('the telemetry token is handed only to a browser session', async ({ playwright, baseURL }) => {
    // A bearer caller (a client-credentials token, as MCP holds) already has
    // its credential; the route must not swap it for another. Minted here
    // rather than taken from global-setup: the mock's tokens last 65 s, and
    // this spec runs late in the suite.
    const mock = await playwright.request.newContext();
    const minted = await mock.post(process.env.OIDC_TOKEN_URL!, {
      form: {
        grant_type: 'client_credentials',
        client_id: process.env.OIDC_CLIENT_ID!,
        client_secret: process.env.OIDC_CLIENT_SECRET!,
        scope: process.env.REQUIRED_SCOPE!,
      },
    });
    const { access_token: token } = await minted.json();
    await mock.dispose();
    const bearer = await playwright.request.newContext({
      baseURL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: { Authorization: `Bearer ${token}` },
    });
    try {
      expect((await bearer.get('/api/me')).status()).toBe(200); // the bearer is good…
      expect((await bearer.get('/api/telemetry/token')).status()).toBe(403); // …but gets no token
    } finally {
      await bearer.dispose();
    }
  });

  test('a session refreshed by the request still gets its token', async ({ page }) => {
    // Drop the access cookie but keep the refresh cookie: the middleware must
    // take its refresh path, and the token it hands out is the rotated one.
    await page.context().clearCookies({ name: 'auth' });
    const res = await page.request.get('/api/telemetry/token');
    expect(res.status()).toBe(200);
    const { access_token: token, expires_in } = await res.json();
    expect(token).toMatch(/^eyJ/);
    expect(expires_in).toBeGreaterThan(30);
    // …and it is the one the browser was just given in its cookie.
    const rotated = (await page.context().cookies()).find((c) => c.name === 'auth');
    expect(rotated).toBeDefined();
  });

  test('a column added after the load is its own trace, not a child of the finished load', async ({
    page,
    request,
  }) => {
    const board = await boardWithCard(request, 'otel-after-load');
    await gotoBoardView(page, board.name);
    await waitFor(() => boardLoad(board.name), 'the board load');
    // A column arriving over SSE mounts a new column view, which fetches its
    // cards — long after the load span ended.
    const later = await apiCreateColumn(request, board.name, 'Later', 1);
    await expect(page.locator('.column-header', { hasText: 'Later' })).toBeVisible();
    const fetch = await waitFor(
      () =>
        readReceiver().spans.find(
          (s) =>
            service(s.resource) === 'bored-spa' &&
            s.name === 'GET /api/columns/{id}/cards' &&
            // The request's own URL is not exported; the server span under it
            // names the column in `url.path`.
            readReceiver().spans.some(
              (child) =>
                child.parentSpanId === s.spanId &&
                String(attr(child.attributes, 'url.path') ?? '').includes(later.id),
            ),
        ),
      "the new column's card fetch",
    );
    expect(fetch.parentSpanId ?? '').toBe('');
  });

  test('a service.name outside the allowed set is dropped by the ingest', async ({ page }) => {
    // A real session token, as the SPA would get it.
    const tokenRes = await page.request.get('/api/telemetry/token');
    expect(tokenRes.status()).toBe(200);
    expect(tokenRes.headers()['cache-control']).toContain('no-store');
    const { access_token: token } = await tokenRes.json();
    const now = BigInt(Date.now()) * 1_000_000n;
    const hex = (n: number) => [...Array(n)].map(() => Math.floor(Math.random() * 16).toString(16)).join('');
    const body = (serviceName: string, name: string) => ({
      resourceSpans: [
        {
          resource: { attributes: [{ key: 'service.name', value: { stringValue: serviceName } }] },
          scopeSpans: [
            {
              spans: [
                {
                  traceId: hex(32),
                  spanId: hex(16),
                  name,
                  kind: 1,
                  startTimeUnixNano: String(now),
                  endTimeUnixNano: String(now + 1n),
                },
              ],
            },
          ],
        },
      ],
    });
    const forged = `forged-${Date.now()}`;
    const control = `control-${Date.now()}`;
    for (const [svc, name] of [
      ['not-bored', forged],
      ['bored-spa', control],
    ]) {
      const res = await page.request.post(`${INGEST}/v1/traces`, {
        headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
        data: body(svc, name),
      });
      // The ingest answers 200 either way; the drop happens after the answer.
      expect(res.status()).toBe(200);
    }
    // The control arriving proves the path works, so the forged span's
    // absence is a drop and not a delay.
    await waitFor(() => readReceiver().spans.find((s) => s.name === control), 'the control span');
    expect(readReceiver().spans.find((s) => s.name === forged)).toBeUndefined();

    // And without a bearer, nothing gets in at all.
    const anonymous = await page.request.post(`${INGEST}/v1/traces`, {
      headers: { 'Content-Type': 'application/json' },
      data: body('bored-spa', `anonymous-${Date.now()}`),
    });
    expect(anonymous.status()).toBe(401);
  });

  test('a panic is logged with its trace, and flushed before the tab dies', async ({ page, request }) => {
    const board = await boardWithCard(request, 'otel-panic');
    // The page's timers run on a fake clock that only moves when told to, so
    // the 5 s export tick fires exactly when this test says. After the panic
    // the clock stays still: the panic hook's own keepalive flush is then the
    // only way the record can leave.
    await page.clock.install();
    await gotoBoardView(page, board.name);
    // One tick, so the exporter sends the load and holds a token: the panic's
    // flush can only use a token it already has.
    await page.clock.runFor(6_000);
    const screen = await waitFor(() => boardLoad(board.name), 'the first export');
    // Stop the page's clock (installed clocks otherwise keep flowing): from
    // here no interval fires, so the tick cannot send the panic record.
    const pageNow = await page.evaluate(() => Date.now());
    await page.clock.pauseAt(pageNow + 100);

    await page.evaluate(() => window.dispatchEvent(new Event('bored:test-panic')));
    await expect(page.locator('#panic-banner')).toBeVisible();

    // Looked for in the screen's trace — this test's own, since the screen
    // span was found through this test's board — so finding it at all proves
    // the record is linked to the trace of the screen it hit.
    const panicLog = await waitFor(
      () =>
        readReceiver().logs.find(
          (l) =>
            service(l.resource) === 'bored-spa' &&
            attr(l.attributes, 'exception.type') === 'panic' &&
            l.traceId === screen.traceId,
        ),
      "the panic log record, in the screen's trace",
    );
    expect(panicLog.body?.stringValue).toBe('wasm panic');
    // Where it happened — never the panic's message, which can quote user content.
    expect(String(attr(panicLog.attributes, 'code.file.path'))).toContain('panic_banner.rs');
    expect(Number(attr(panicLog.attributes, 'code.line.number'))).toBeGreaterThan(0);
    expect(attr(panicLog.attributes, 'exception.message')).toBeUndefined();
    // And to its span: the panic span, filed under the screen it hit.
    const panicSpan = await waitFor(
      () => readReceiver().spans.find((s) => s.spanId === panicLog.spanId && s.name === 'panic'),
      'the panic span',
    );
    expect(panicSpan.parentSpanId).toBe(screen.spanId);
  });

  test('leaving the page flushes what is buffered (keepalive, not the 5 s tick)', async ({ page, request }) => {
    const board = await boardWithCard(request, 'otel-unload');
    // A second board only the chooser's own fetch can show: seeing it proves
    // that fetch — and so the chooser's span — has finished.
    const other = await apiCreateBoard(request, `otel-unload-other-${Date.now()}`);
    // As in the panic test: the page's timers run on a fake clock, so the 5 s
    // tick fires only when this test says.
    await page.clock.install();
    await gotoBoardView(page, board.name);
    // One tick, so the exporter sends the load and holds a token: the unload
    // flush can only use a token it already has.
    await page.clock.runFor(6_000);
    await waitFor(() => boardLoad(board.name), 'the first export (so a token is cached)');
    // Stop the clock: from here no interval fires, so the tick cannot send the
    // chooser's span — the unload flush is the only way it can leave.
    const pageNow = await page.evaluate(() => Date.now());
    await page.clock.pauseAt(pageNow + 100);

    // Something new, then leave. The mark is read from the page's own
    // telemetry clock, not the runner's (see `boardLoad`'s preamble).
    const beforeChooser = await pageClockMs(page);
    await openChooser(page);
    await expect(page.locator('.board-chooser')).toContainText(other.name);
    await page.goto('about:blank');

    await waitFor(
      () =>
        readReceiver().spans.find(
          (s) =>
            service(s.resource) === 'bored-spa' &&
            s.name === 'screen board chooser' &&
            atOrAfter(s.startTimeUnixNano, beforeChooser),
        ),
      'the chooser span, sent on the way out',
      15_000,
    );
  });

  for (const refusal of ['401', '503', 'network'] as const) {
    test(`the SPA stays fully usable with the ingest refusing everything (${refusal})`, async ({ page, request }) => {
      const board = await boardWithCard(request, `otel-refuse-${refusal}`);
      const crashes = watchCrashes(page);
      const telemetryLines = watchTelemetryConsole(page);
      // The ingest is another origin here, so a refusal the page can *read*
      // (rather than an opaque CORS failure) carries the CORS headers the real
      // ingest sends, and the preflight is allowed.
      const cors = {
        'Access-Control-Allow-Origin': 'https://app',
        'Access-Control-Allow-Headers': 'authorization, content-type',
        'Access-Control-Allow-Methods': 'POST',
        'Access-Control-Expose-Headers': 'Retry-After',
      };
      await page.route(`${INGEST}/v1/**`, (route) => {
        if (refusal === 'network') return route.abort();
        if (route.request().method() === 'OPTIONS') return route.fulfill({ status: 204, headers: cors });
        return route.fulfill({
          status: Number(refusal),
          contentType: 'application/json',
          headers: { ...cors, ...(refusal === '503' ? { 'Retry-After': '1' } : {}) },
          body: JSON.stringify({ code: 16, message: refusal === '401' ? 'invalid token: expired' : 'not ready' }),
        });
      });
      await gotoBoardView(page, board.name);

      // Let the exporter meet the refusal (first tick is within 5 s).
      await expect.poll(() => telemetryLines.length, { timeout: 20_000 }).toBeGreaterThan(0);
      // The app is untouched: a card can be created and edited, and the edit
      // reaches the server.
      await page.locator('[title="Add card"]').first().click();
      await expect(page.locator('.card-item.card-expanded')).toBeVisible();
      const textarea = page.locator('.card-body-textarea').first();
      await expect(textarea).toBeVisible();
      await textarea.fill('Made while telemetry fails');
      await textarea.press('Escape');
      await expect(page.locator('.card-item', { hasText: 'Made while telemetry fails' })).toBeVisible();
      await expect
        .poll(async () => {
          const res = await request.get(`/api/boards/${board.name}/columns`);
          const [column] = await res.json();
          const cards = await (await request.get(`/api/columns/${column.id}/cards`)).json();
          return cards.some((c: { body: string }) => c.body === 'Made while telemetry fails');
        })
        .toBe(true);
      // And it still reacts: filtering repaints the column.
      await page.locator('.navbar-search-input').fill('Telemetry card');
      await expect(page.locator('.card-item', { hasText: 'Telemetry card' })).toBeVisible();
      await expect(page.locator('.card-item', { hasText: 'Made while telemetry fails' })).toBeHidden();

      if (refusal === '401') {
        // The edits above buffered more telemetry, so a second export is
        // attempted with a refreshed token; its 401 stops export for the
        // session — one refresh, never a loop.
        await expect
          .poll(() => telemetryLines.some((l) => l.includes('giving up')), { timeout: 20_000 })
          .toBe(true);
      }

      expect(crashes).toEqual([]);
      // Transitions only: never a line per batch.
      expect(telemetryLines.length).toBeLessThanOrEqual(2);
      for (const line of telemetryLines) expect(line).not.toMatch(/eyJ/); // never a JWT
    });
  }

  test('a 401 on one signal refetches the token before the next signal is sent', async ({ page, request }) => {
    const board = await boardWithCard(request, 'otel-refetch');
    const cors = {
      'Access-Control-Allow-Origin': 'https://app',
      'Access-Control-Allow-Headers': 'authorization, content-type',
      'Access-Control-Allow-Methods': 'POST',
    };
    // Traces are refused, logs accepted: the pass that meets the traces 401
    // must fetch a token again before it sends the logs batch.
    const order: string[] = [];
    page.on('request', (req) => {
      const path = new URL(req.url()).pathname;
      if (path === '/api/telemetry/token') order.push('token');
    });
    await page.route(`${INGEST}/v1/**`, (route) => {
      if (route.request().method() === 'OPTIONS') return route.fulfill({ status: 204, headers: cors });
      const logs = route.request().url().endsWith('/v1/logs');
      order.push(logs ? 'logs' : 'traces');
      return route.fulfill({
        status: logs ? 200 : 401,
        contentType: 'application/json',
        headers: cors,
        body: logs ? '{}' : JSON.stringify({ code: 16, message: 'invalid token: expired' }),
      });
    });
    // A card number that does not exist: its failed fetch is an error log,
    // so the first pass has both a traces and a logs batch.
    await page.goto(`/boards/${board.name}?card=99999999`);
    await expect.poll(() => order.includes('logs'), { timeout: 20_000 }).toBe(true);
    const firstTraces = order.indexOf('traces');
    const firstLogs = order.indexOf('logs');
    expect(firstTraces).toBeGreaterThanOrEqual(0);
    expect(order.slice(firstTraces + 1, firstLogs)).toContain('token');
  });

  test('without telemetry configuration OTLP is never initialised', async ({ page, request }) => {
    const board = await boardWithCard(request, 'otel-off');
    const telemetryLines = watchTelemetryConsole(page);
    const telemetryRequests: string[] = [];
    page.on('request', (req) => {
      const path = new URL(req.url()).pathname;
      if (path.startsWith('/v1/') || path === '/api/telemetry/token') telemetryRequests.push(path);
    });
    // The server says nothing about telemetry — an older server, or an
    // environment without the ingest.
    await page.route('**/api/info', async (route) => {
      const res = await route.fetch();
      const info = await res.json();
      delete info.telemetry;
      await route.fulfill({ response: res, json: info });
    });
    await gotoBoardView(page, board.name);
    await expect
      .poll(() => telemetryLines.some((l) => l.includes('OTLP not initialised')), { timeout: 15_000 })
      .toBe(true);
    // Two ticks' worth of time, and nothing tried to export.
    await page.waitForTimeout(11_000);
    expect(telemetryRequests).toEqual([]);
    expect(telemetryLines).toHaveLength(1);
    await expect(page.locator('.card-item', { hasText: 'Telemetry card' })).toBeVisible();
  });
});
