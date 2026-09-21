import { test, expect } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, apiUpdateCard, gotoBoardView } from './helpers';

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

  test('does not reload when the reload could not be remembered', async ({ page, request }) => {
    // The loop guard is a marker in sessionStorage. Where storage is blocked —
    // a browser setting, a privacy mode — the tab could not tell its second
    // reload from its first, and a mismatch that never resolves would reload it
    // on every heartbeat, forever. So no usable storage means no reload at all.
    //
    // Blocked the way real browsers block it: the object is there, and throws
    // the moment it is written to.
    await page.addInitScript(() => {
      Storage.prototype.setItem = () => {
        throw new DOMException('blocked for this test', 'SecurityError');
      };
    });
    const board = await apiCreateBoard(request, `no-storage-${Date.now()}`);
    // Mismatched from the very first heartbeat.
    await page.route('**/api/info', (route) =>
      route.fulfill({ json: { version: '99.99.0', env: 'test' } }),
    );
    await gotoBoardView(page, board.name);

    let loads = 0;
    page.on('load', () => { loads += 1; });

    // The anchor that keeps the assertion below from being vacuous: the
    // watermark follows the heartbeat, so 99.99.0 here proves a heartbeat ran,
    // saw the mismatch, and the tab is still standing.
    await expect(page.locator('.navbar-watermark')).toContainText('99.99.0', { timeout: 15000 });
    await page.waitForTimeout(4000);
    expect(loads).toBe(0);
    // Staying put must not cost the user the board.
    await expect(page.locator('.columns-row')).toBeVisible();
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

// Home is the only page with no event stream, which makes it the only place
// two things can be observed at all: the heartbeat-only offline state, and the
// stream state being forgotten when a board is left behind.
//
// It redirects to the first board whenever one exists, and this suite's
// database always has some — so these specs answer the board *list* with `[]`
// to hold home on its empty state. Only that one GET is mocked; the glob ends
// at `/api/boards`, so `/api/boards/<slug>` and everything under it is real.
test.describe('Home, which has no event stream', () => {
  const noBoards = (route: import('@playwright/test').Route) =>
    route.request().method() === 'GET' ? route.fulfill({ json: [] }) : route.fallback();

  test('goes offline on the heartbeat alone, and its create form is inert', async ({ page }) => {
    let streams = 0;
    page.on('request', (req) => {
      if (new URL(req.url()).pathname === '/api/events') streams += 1;
    });
    await page.route('**/api/boards', noBoards);
    await page.route('**/api/info', (route) => route.abort());
    await page.goto('/');
    await expect(page.locator('.empty-state')).toBeVisible();

    await expect(page.locator('.navbar-connection')).toBeVisible({ timeout: 15000 });
    // Nothing but the heartbeat can have done that: this page never opened a
    // stream to lose.
    expect(streams).toBe(0);
    await expect(page.locator('.create-form button')).toHaveCSS('pointer-events', 'none');
  });

  test('leaving a board whose stream is dead does not leave home offline', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `leave-dead-stream-${Date.now()}`);
    await page.route('**/api/events*', (route) => route.abort());
    await gotoBoardView(page, board.name);
    // Offline because of the stream, and only the stream: /api/info is
    // untouched, so the heartbeat is healthy throughout.
    await expect(page.locator('.navbar-connection')).toBeVisible({ timeout: 15000 });

    // A client-side navigation, proven rather than assumed: a full page load
    // would reset all state and clear the badge for free, and this spec would
    // pass without the code it is here for.
    let loads = 0;
    page.on('load', () => { loads += 1; });
    await page.route('**/api/boards', noBoards);
    await page.locator('.navbar-brand').click();
    await expect(page.locator('.empty-state')).toBeVisible();

    // The dead stream belonged to the board. Home has none, so its health is
    // the heartbeat's alone — and the heartbeat is fine.
    await expect(page.locator('.navbar-connection')).toHaveCount(0, { timeout: 5000 });
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

    // Delete in particular, because its refusal is otherwise invisible: the
    // confirm dialog would close, the card would stay, and only the console
    // would know why. The card is still expanded from the edit above, so its
    // toolbar — and the delete button in it — is on screen.
    const deleteButton = page.locator('.card-item.card-expanded .card-toolbar-close');
    await expect(deleteButton).toBeVisible();
    await expect(deleteButton).toHaveCSS('pointer-events', 'none');
    // `force` skips Playwright's own actionability wait, which would otherwise
    // time out on exactly the property under test; the browser still honours
    // `pointer-events: none`, so the click lands on nothing.
    await deleteButton.click({ force: true });
    await expect(page.locator('.btn-danger')).toHaveCount(0);
    await expect(page.locator('.card-item')).toHaveCount(1);
  });

  test('a server that accepts the heartbeat and never answers is offline too', async ({ page, request }) => {
    // The outage a bare `await` cannot see: no refusal, no 502, just silence.
    // The route handler below never fulfils, continues or aborts, so every
    // /api/info request hangs exactly as it would against a wedged container.
    //
    // The event stream is deliberately left alone. With the stream healthy the
    // heartbeat is the *only* thing that can mark this tab disconnected — so
    // the badge appearing is the request deadline firing and nothing else.
    // Without the deadline the heartbeat awaits forever and this never shows.
    const board = await apiCreateBoard(request, `hung-heartbeat-${Date.now()}`);
    await page.route('**/api/info', () => { /* never answered */ });
    await gotoBoardView(page, board.name);

    await expect(page.locator('.navbar-connection')).toBeVisible({ timeout: 15000 });

    // Liveness: the tab recovers once the server answers again, which also
    // proves the heartbeat loop survived the abort rather than dying with it.
    await page.unroute('**/api/info');
    await expect(page.locator('.navbar-connection')).toHaveCount(0, { timeout: 20000 });
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

  test('a late failure does not undo a later reorder the server accepted', async ({ page, request }) => {
    // Two drags, and the *first* request fails only after the second has been
    // accepted. Undoing the first by restoring its pre-drag snapshot would undo
    // the second as well, and leave the board on an order the server does not
    // have. The rollback has to end on the server's actual order.
    const board = await apiCreateBoard(request, `reorder-race-${Date.now()}`);
    await apiCreateColumn(request, board.name, 'A', 0);
    await apiCreateColumn(request, board.name, 'B', 1);
    await apiCreateColumn(request, board.name, 'C', 2);
    await gotoBoardView(page, board.name);
    await expect(page.locator('.column-name')).toHaveText(['A', 'B', 'C']);

    // Hold the first reorder request until released, then fail it. Every later
    // one goes through to the real server.
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    let reorders = 0;
    let firstHeld!: () => void;
    const held = new Promise<void>((resolve) => { firstHeld = resolve; });
    await page.route('**/columns/reorder', async (route) => {
      reorders += 1;
      if (reorders === 1) {
        firstHeld();
        await gate;
        await route.fulfill({ status: 500, contentType: 'text/plain', body: 'boom' });
      } else {
        await route.continue();
      }
    });

    // Drag 1: B to the front -> [B, A, C]. Its request hangs.
    await page.locator('.column-grip').nth(1).dragTo(
      page.locator('.column-view').nth(0).locator('.card-list'),
    );
    await held;
    await expect(page.locator('.column-name')).toHaveText(['B', 'A', 'C']);

    // Drag 2: C before A -> [B, C, A]. The server accepts this one.
    const accepted = page.waitForResponse(
      (res) => res.url().includes('/columns/reorder') && res.status() === 200,
    );
    await page.locator('.column-grip').nth(2).dragTo(
      page.locator('.column-view').nth(1).locator('.card-list'),
    );
    await accepted;
    await expect(page.locator('.column-name')).toHaveText(['B', 'C', 'A']);

    // Now the first request fails, late.
    const failed = page.waitForResponse(
      (res) => res.url().includes('/columns/reorder') && res.status() === 500,
    );
    release();
    await failed;

    // Settle on what the server holds, which is drag 2's order — asserted
    // against the server itself, not a hardcoded expectation.
    const serverNames = async () =>
      ((await (await request.get(`/api/boards/${board.name}/columns`)).json()) as { name: string }[])
        .map((c) => c.name);
    expect(await serverNames()).toEqual(['B', 'C', 'A']);
    // Give the rollback its refetch, then require the board to match. A
    // snapshot restore would have put it back to [A, B, C].
    await page.waitForTimeout(1500);
    await expect(page.locator('.column-name')).toHaveText(await serverNames());
  });

  test('catches up after a gap the browser recovers from by itself', async ({ page, request }) => {
    // The common real-world gap — a network blip, a laptop waking — is not a
    // CLOSED stream. It is a transport failure the browser retries on its own
    // (`readyState` CONNECTING), so the reopen comes from the browser, not the
    // client's supervisor. The board has missed events all the same, and must
    // reload rather than resume.
    const board = await apiCreateBoard(request, `native-retry-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Original body');

    // Aborted rather than 502'd: a failed connection is exactly what the
    // browser retries by itself.
    await page.route('**/api/events*', (route) => route.abort());
    await gotoBoardView(page, board.name);
    await expect(page.locator('.navbar-connection')).toBeVisible({ timeout: 15000 });

    await apiUpdateCard(request, card.id, { body: 'Changed during the blip' });

    let loads = 0;
    page.on('load', () => { loads += 1; });
    const resumed = page.waitForEvent('load', { timeout: 30000 });
    await page.unroute('**/api/events*');
    await resumed;

    await expect(page.locator('.card-item').first()).toContainText('Changed during the blip');
    await expect(page.locator('.navbar-connection')).toHaveCount(0, { timeout: 15000 });
    await page.waitForTimeout(3000);
    expect(loads).toBe(1);
  });

  test('catches up on what it missed before accepting saves again', async ({ page, request }) => {
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
    await expect(page.locator('.card-item').first()).toContainText('Original body');

    // Someone else changes the card while this tab cannot hear about it. The
    // backend keeps no event history, so this `CardUpdated` is lost to the tab
    // for good — the one way to learn of it is to load the board again.
    await apiUpdateCard(request, card.id, { body: 'Changed while you were away' });

    let loads = 0;
    page.on('load', () => { loads += 1; });
    const resumed = page.waitForEvent('load', { timeout: 30000 });

    // Server's back. The stream must be reopened by the client's own supervisor
    // — the browser will not do it after a CLOSED stream — and a stream that
    // comes back after a gap reloads the tab rather than resuming on a board
    // that is missing the change above.
    await page.unroute('**/api/info');
    await page.unroute('**/api/events*');
    await resumed;

    // The oracle: the change made during the gap is on screen *before* the tab
    // is writable. Resuming without reloading would clear the badge on a board
    // that still reads "Original body".
    await expect(page.locator('.card-item').first()).toContainText('Changed while you were away');
    await expect(page.locator('.navbar-connection')).toHaveCount(0, { timeout: 15000 });

    // A save works again, end to end — on top of the current body, not the
    // stale one.
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();
    await page.locator('.card-body-rendered').first().click();
    const textarea = page.locator('.card-body-textarea').first();
    await expect(textarea).toBeVisible();
    await expect(textarea).toHaveValue('Changed while you were away');
    await textarea.fill('Edited after reconnecting');
    await textarea.press('Escape');
    await expect(page.locator('.card-markdown').first()).toContainText('Edited after reconnecting');

    await expect.poll(async () => {
      const res = await request.get(`/api/cards/${card.id}`);
      return (await res.json()).body;
    }, { timeout: 10000 }).toBe('Edited after reconnecting');

    // One reload to catch up, not one per heartbeat: the fresh page's stream
    // opened cleanly, so it resumed normally.
    expect(loads).toBe(1);
  });
});
