import { test, expect, Page } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView } from './helpers';

// ── #405 Deploy reload + disconnected UI ──────────────────────────────────
//
// Every spec here drives the real path: the `/api/info` heartbeat and the SSE
// stream, through routes that fail the way a redeploy or an outage fails. The
// suite these replace (iteration 16, #24) called the EventSource's `onerror`
// and `onopen` by hand, which is a sequence a real deploy never produces — the
// proxy answers 502 while the container restarts, the browser marks the stream
// CLOSED and never retries, and the version check never ran.

/**
 * The version the running bundle was built with, read off the navbar watermark
 * (`v1.58.0` or `v1.58.0 some-branch`).
 *
 * Deliberately read from the page rather than computed here: the value is
 * baked into the image at build time, so hardcoding it would make this suite
 * fail on every release, and deriving it from Cargo.toml would reimplement the
 * very equality the reload depends on.
 */
async function bundleVersion(page: Page): Promise<string> {
  const label = await page.locator('.navbar-watermark').innerText();
  return label.replace(/^v/, '').split(' ')[0];
}

test.describe('Reload when the server is redeployed', () => {
  test('reloads onto the new version, and only once', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `deploy-reload-${Date.now()}`);
    await gotoBoardView(page, board.name);
    const before = await bundleVersion(page);
    expect(before).not.toBe('99.99.0');

    // Count real document loads so "reloaded" and "looped" are distinguishable.
    let loads = 0;
    page.on('load', () => { loads += 1; });

    // From here the server claims to be a different build — what a redeploy
    // looks like to an open tab.
    await page.route('**/api/info', (route) =>
      route.fulfill({ json: { version: '99.99.0', env: 'test' } }),
    );

    // Nothing else is touched: the stream stays up and the tab keeps working.
    // The heartbeat alone has to notice, which is the property the previous
    // implementation lacked — it could only check on an SSE reconnect that a
    // real deploy never delivers.

    // The tab reloads itself, same URL, onto what the server now reports.
    await expect(page.locator('.navbar-watermark')).toContainText('99.99.0', { timeout: 20000 });
    expect(page.url()).toContain(`/boards/${board.name}`);
    // And comes back a working board rather than a blank page.
    await expect(page.locator('.columns-row')).toBeVisible();

    // The reloaded bundle is still not 99.99.0 — the mock keeps lying — so a
    // client without the loop guard would reload forever. Exactly one load.
    await page.waitForTimeout(4000);
    expect(loads).toBe(1);
  });

  test('does not reload while the server version is unchanged', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `no-reload-${Date.now()}`);
    // Routed before the page loads: a `page.route` only intercepts requests
    // made after it is installed, and an SSE stream that is already open stays
    // open regardless.
    await page.route('**/api/events*', (route) => route.abort());
    await gotoBoardView(page, board.name);

    let loads = 0;
    page.on('load', () => { loads += 1; });

    // The stream is dead, so the client probes /api/info — and finds the same
    // build it is running. It must say so and stay put.
    await expect(page.locator('.navbar-connection')).toBeVisible({ timeout: 15000 });

    // Long enough to cover several heartbeats at the backoff intervals.
    await page.waitForTimeout(8000);
    expect(loads).toBe(0);
  });
});

test.describe('Disconnected UI', () => {
  test('shows the offline badge and refuses to save while disconnected', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `offline-badge-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Original body');
    await gotoBoardView(page, board.name);
    // A healthy tab says nothing — the badge is not just always on.
    await expect(page.locator('.navbar-connection')).toHaveCount(0);

    // Take the server away entirely: no event stream, no heartbeat. Routes only
    // bite on new requests, so reload to make the live stream re-open into
    // them — the same fresh start a user gets when the server dies under a
    // long-lived tab.
    await page.route('**/api/events*', (route) => route.abort());
    await page.route('**/api/info', (route) => route.abort());
    await page.reload();
    await page.waitForSelector('.columns-row');

    await expect(page.locator('.navbar-connection')).toBeVisible({ timeout: 15000 });

    // No mutation may reach the server while in this state — not even one the
    // user explicitly asks for. Watch for the PUT as well as its effect.
    let mutations = 0;
    page.on('request', (req) => {
      if (req.method() !== 'GET' && new URL(req.url()).pathname.startsWith('/api/')) {
        mutations += 1;
      }
    });

    // Expanding and editing are reads — a disconnected tab is still worth
    // using. It is the save at the end that must be refused.
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();
    await page.locator('.card-body-rendered').first().click();
    const textarea = page.locator('.card-body-textarea').first();
    await expect(textarea).toBeVisible();
    await textarea.fill('Edited while offline');
    await textarea.press('Escape');

    // The card reports the refusal rather than pretending to have saved.
    await expect(page.locator('.card-save-icon').first()).toHaveText('!', { timeout: 10000 });
    expect(mutations).toBe(0);

    // And the server never heard about it.
    const check = await request.get(`/api/columns/${col.id}/cards`);
    const cards = await check.json();
    expect(cards[0].body).toBe('Original body');

    // Mutation affordances are visibly unavailable, not merely inert: a
    // control that still looks live but silently does nothing is the failure
    // mode this iteration is about.
    await expect(page.locator('.add-card-btn').first()).toHaveCSS('pointer-events', 'none');
  });

  test('clears the badge and accepts saves once the server comes back', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `offline-recovery-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Original body');

    // 502 rather than abort, so the stream ends up CLOSED: after a response
    // that is not `text/event-stream` the browser gives up permanently, and
    // only the client's own supervisor can bring the stream back. This is the
    // failure the old implementation could not recover from.
    await page.route('**/api/events*', (route) =>
      route.fulfill({ status: 502, contentType: 'text/html', body: '<html>bad gateway</html>' }),
    );
    await page.route('**/api/info', (route) => route.abort());
    await gotoBoardView(page, board.name);
    await expect(page.locator('.navbar-connection')).toBeVisible({ timeout: 15000 });

    // Server's back. The heartbeat must recover on its own, and the stream
    // must be reopened by the client's own supervisor — the browser will not
    // do it after a CLOSED stream.
    await page.unroute('**/api/info');
    await page.unroute('**/api/events*');

    await expect(page.locator('.navbar-connection')).toHaveCount(0, { timeout: 30000 });

    // A save works again, end to end.
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();
    await page.locator('.card-body-rendered').first().click();
    const textarea = page.locator('.card-body-textarea').first();
    await expect(textarea).toBeVisible();
    await textarea.fill('Edited after reconnecting');
    await textarea.press('Escape');
    await expect(page.locator('.card-markdown').first()).toContainText('Edited after reconnecting');

    await expect.poll(async () => {
      const res = await request.get(`/api/cards/${card.id}`);
      return (await res.json()).body;
    }, { timeout: 10000 }).toBe('Edited after reconnecting');
  });
});
