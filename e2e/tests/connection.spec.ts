import { test, expect } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView } from './helpers';

// ── #405 Deploy reload + disconnected UI ──────────────────────────────────
//
// Every spec here drives the real path: the `/api/info` heartbeat and the SSE
// stream, through routes that fail the way a redeploy or an outage fails. The
// suite these replace (iteration 16, #24) called the EventSource's `onerror`
// and `onopen` by hand, which is a sequence a real deploy never produces — the
// proxy answers 502 while the container restarts, the browser marks the stream
// CLOSED and never retries, and the version check never ran.

test.describe('Reload when the server is redeployed', () => {
  test('reloads onto the new version, and only once', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `deploy-reload-${Date.now()}`);
    await gotoBoardView(page, board.name);

    // Count real document loads. This is the oracle for the whole spec: a
    // `load` event is the one thing a reload produces and a re-render cannot.
    let loads = 0;
    page.on('load', () => { loads += 1; });
    const reloaded = page.waitForEvent('load', { timeout: 20000 });

    // From here the server claims to be a different build — what a redeploy
    // looks like to an open tab. Nothing else is touched: the stream stays up
    // and the tab keeps working. The heartbeat alone has to notice, which is
    // the property the previous implementation lacked — it could only check on
    // an SSE reconnect that a real deploy never delivers.
    await page.route('**/api/info', (route) =>
      route.fulfill({ json: { version: '99.99.0', env: 'test' } }),
    );

    // THE RELOAD. The tab reloads itself, and onto the same URL.
    await reloaded;
    expect(page.url()).toContain(`/boards/${board.name}`);
    // It comes back a working board rather than a blank page.
    await expect(page.locator('.columns-row')).toBeVisible();

    // Secondary, and not evidence of a reload by itself: the watermark renders
    // whatever version the heartbeat last fetched, so it would read 99.99.0
    // here with or without one. It is checked because a watermark that did
    // *not* follow the server is the stale-titlebar half of card #405.
    await expect(page.locator('.navbar-watermark')).toContainText('99.99.0');

    // THE LOOP GUARD. The reloaded bundle is still not 99.99.0 — the mock keeps
    // lying — so a client with no guard would reload again on its first
    // heartbeat, and again after that. Still exactly one load, well past it.
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

  test('refuses to add a card link while disconnected, and says why', async ({ page, request }) => {
    // The card-link routes have a guard of their own (`offline_guard_link` in
    // api.rs): they report failures as a `LinkApiError`, which the link editor
    // shows to the user, rather than the `gloo_net::Error` every other route
    // uses. A separate guard on a separate UI path needs its own proof.
    const board = await apiCreateBoard(request, `offline-link-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, '# Alpha card');
    const beta = await apiCreateCard(request, col.id, '# Beta card');

    await page.route('**/api/events*', (route) => route.abort());
    await page.route('**/api/info', (route) => route.abort());
    await gotoBoardView(page, board.name);
    await expect(page.locator('.navbar-connection')).toBeVisible({ timeout: 15000 });

    let mutations = 0;
    page.on('request', (req) => {
      if (req.method() !== 'GET' && new URL(req.url()).pathname.startsWith('/api/')) {
        mutations += 1;
      }
    });

    // Picking a card to link is a read, and works. Committing the pick is the
    // mutation.
    await page.locator('.card-item', { hasText: 'Alpha card' }).click();
    const after = page.locator('.link-group[data-side="after"]');
    await after.locator('.link-picker-input').fill('beta');
    const option = page.locator('.link-suggestions .link-suggestion');
    await expect(option).toHaveText(`#${beta.number} Beta card`);
    await option.click();

    // The editor explains the refusal in words — the exact text, because the
    // failure this guards against is a bare status code or an empty alert.
    await expect(page.locator('.link-editor-error')).toHaveText(
      'disconnected from the server — changes are not being saved',
    );
    // No chip was drawn for a link that does not exist, nothing was sent, and
    // the server has no such link.
    await expect(page.locator('.link-chip')).toHaveCount(0);
    expect(mutations).toBe(0);
    const links = await (await request.get(`/api/boards/${board.name}/links`)).json();
    expect(links).toEqual([]);
  });

  test('a column reorder the server does not accept is undone', async ({ page, request }) => {
    // The column drag is the one optimistic write among the drag handlers: the
    // list is reordered locally before the request goes out. A refusal — the
    // offline guard in api.rs, or a server failure — has to put it back, or
    // the board shows an order the server never took until the next reload.
    //
    // Driven through a 500 rather than the offline state on purpose. Both
    // arrive at the same `Err` arm, but offline the drag cannot even start in
    // Chromium (`-webkit-user-drag: none`), which is the only browser this
    // suite runs — so the offline route could not reach the code under test.
    const board = await apiCreateBoard(request, `reorder-rollback-${Date.now()}`);
    await apiCreateColumn(request, board.name, 'Alpha', 0);
    await apiCreateColumn(request, board.name, 'Beta', 1);
    await gotoBoardView(page, board.name);
    await expect(page.locator('.column-name')).toHaveText(['Alpha', 'Beta']);

    await page.route('**/columns/reorder', (route) =>
      route.fulfill({ status: 500, contentType: 'text/plain', body: 'boom' }),
    );

    // Wait for the refusal itself before looking at the order: [Alpha, Beta] is
    // also the order *before* the drag, so asserting it without this anchor
    // would pass on a drag that never happened.
    const refused = page.waitForResponse(
      (res) => res.url().includes('/columns/reorder') && res.status() === 500,
    );
    await page.locator('.column-grip').nth(1).dragTo(
      page.locator('.column-view').nth(0).locator('.card-list'),
    );
    await refused;

    await expect(page.locator('.column-name')).toHaveText(['Alpha', 'Beta']);
    // The server agrees that nothing moved.
    const columns = await (await request.get(`/api/boards/${board.name}/columns`)).json();
    expect(columns.map((c: { name: string }) => c.name)).toEqual(['Alpha', 'Beta']);

    // Liveness: the undo left a working board, not a wedged one. The same drag
    // goes through once the server accepts it.
    await page.unroute('**/columns/reorder');
    await page.locator('.column-grip').nth(1).dragTo(
      page.locator('.column-view').nth(0).locator('.card-list'),
    );
    await expect(page.locator('.column-name')).toHaveText(['Beta', 'Alpha']);
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
