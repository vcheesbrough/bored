import { test, expect, Browser } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateColumn,
  apiCreateCard,
  apiUpdateCard,
  gotoBoardView,
  openChooser,
} from './helpers';

// All SSE tests use two independent browser contexts (A and B) connected to the
// same board. Context A performs a mutation; context B must reflect it without
// any manual reload.

test.describe('SSE real-time updates', () => {
  test('card created in context A appears in context B', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `sse-create-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.name);

    // Context A clicks the + button to create a card.
    await pageA.locator('[title="Add card"]').first().click();
    await expect(pageA.locator('.card-item')).toHaveCount(1);

    // Context B should receive the SSE event and show the new card.
    await expect(pageB.locator('.card-item')).toHaveCount(1, { timeout: 5000 });

    await ctxA.close();
    await ctxB.close();
  });

  test('card body edited in context A updates in context B', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `sse-edit-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Original body');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.name);

    // Expand, edit and save in context A.
    await pageA.locator('.card-item').first().click();
    await pageA.locator('.card-body-rendered').first().click();
    await pageA.locator('.card-body-textarea').first().fill('Updated body');
    await pageA.locator('.card-body-textarea').first().press('Escape');

    // Context B should see the update reflected in the card preview.
    await expect(pageB.locator('.card-preview').first()).toContainText('Updated body', {
      timeout: 5000,
    });

    await ctxA.close();
    await ctxB.close();
  });

  test('card deleted in context A disappears in context B', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `sse-delete-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Goodbye');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.name);

    await expect(pageB.locator('.card-item')).toHaveCount(1);

    // Delete the card in context A.
    await pageA.locator('.card-item').first().click();
    await pageA.locator('.card-toolbar-close').first().click();
    await expect(pageA.locator('.confirm-dialog')).toBeVisible();
    await pageA.locator('.btn-danger').click();

    // Context B should no longer see the card.
    await expect(pageB.locator('.card-item')).toHaveCount(0, { timeout: 5000 });

    await ctxA.close();
    await ctxB.close();
  });

  test('card moved in context A updates column in context B', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `sse-move-board-${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.name, 'Source', 0);
    const col2 = await apiCreateColumn(request, board.name, 'Target', 1);
    await apiCreateCard(request, col1.id, 'Moving card');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.name);

    await expect(pageB.locator('.column-view').nth(0).locator('.card-item')).toHaveCount(1);
    await expect(pageB.locator('.column-view').nth(1).locator('.card-item')).toHaveCount(0);

    // Drag in context A.
    const cardEl = pageA.locator('.column-view').nth(0).locator('.card-item').first();
    const targetList = pageA.locator('.column-view').nth(1).locator('.card-list');
    await cardEl.dragTo(targetList);

    // Context B should see the card in the target column.
    await expect(pageB.locator('.column-view').nth(1).locator('.card-item')).toHaveCount(1, {
      timeout: 5000,
    });
    await expect(pageB.locator('.column-view').nth(0).locator('.card-item')).toHaveCount(0, {
      timeout: 5000,
    });

    await ctxA.close();
    await ctxB.close();
  });

  test('column created in context A appears in context B', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `sse-col-create-board-${Date.now()}`);

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.name);

    // Create a column via the API (which the backend broadcasts via SSE).
    await apiCreateColumn(request, board.name, 'New Column');

    await expect(pageA.locator('.column-name').filter({ hasText: 'New Column' })).toBeVisible({
      timeout: 5000,
    });
    await expect(pageB.locator('.column-name').filter({ hasText: 'New Column' })).toBeVisible({
      timeout: 5000,
    });

    await ctxA.close();
    await ctxB.close();
  });

  test('column renamed in context A updates in context B', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `sse-col-rename-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Original Name');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.name);

    await expect(pageB.locator('.column-name').filter({ hasText: 'Original Name' })).toBeVisible();

    // Rename via API (broadcasts SSE); mirrors what the chooser UI calls.
    await request.put(`/api/columns/${col.id}`, { data: { name: 'Renamed Column' } });

    await expect(pageA.locator('.column-name').filter({ hasText: 'Renamed Column' })).toBeVisible({
      timeout: 5000,
    });
    await expect(pageB.locator('.column-name').filter({ hasText: 'Renamed Column' })).toBeVisible({
      timeout: 5000,
    });
    await expect(pageB.locator('.column-name').filter({ hasText: 'Original Name' })).not.toBeVisible();

    await ctxA.close();
    await ctxB.close();
  });

  test('column deleted in context A disappears in context B', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `sse-col-delete-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Delete Column');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.name);

    await expect(pageB.locator('.column-name').filter({ hasText: col.name })).toBeVisible();

    // Delete the column in context A via the chooser.
    await pageA.locator('.navbar-board-btn').click();
    await pageA.waitForSelector('.board-chooser', { state: 'visible' });
    pageA.once('dialog', dialog => dialog.accept());
    await pageA
      .locator('.chooser-col-row')
      .filter({ hasText: col.name })
      .locator('.chooser-col-delete')
      .click();

    // Context B should no longer show the column.
    await expect(pageB.locator('.column-name').filter({ hasText: col.name })).not.toBeVisible({
      timeout: 5000,
    });

    await ctxA.close();
    await ctxB.close();
  });
});

// ── Helpers ───────────────────────────────────────────────────────────────

async function openTwoContexts(browser: Browser) {
  const baseURL = process.env.BASE_URL;
  const ctxA = await browser.newContext({ baseURL, ignoreHTTPSErrors: true });
  const ctxB = await browser.newContext({ baseURL, ignoreHTTPSErrors: true });
  return [ctxA, ctxB] as const;
}

async function openBoardInBoth(
  ctxA: Awaited<ReturnType<Browser['newContext']>>,
  ctxB: Awaited<ReturnType<Browser['newContext']>>,
  boardSlug: string
) {
  const pageA = await ctxA.newPage();
  const pageB = await ctxB.newPage();
  await gotoBoardView(pageA, boardSlug);
  await gotoBoardView(pageB, boardSlug);
  return [pageA, pageB] as const;
}

// Card #452. `BoardView` stays mounted across `/boards/:slug`, and its
// `sse_event` signal keeps the last event it saw. A column mounting on a
// return visit must not replay that event over the fresh snapshot: the board
// may have changed while this tab was elsewhere and could not hear about it.
test.describe('returning to a board', () => {
  test('an event from an earlier visit does not overwrite fresher data', async ({ page, request }) => {
    const boardA = await apiCreateBoard(request, `stale-a-${Date.now()}`);
    const boardB = await apiCreateBoard(request, `stale-b-${Date.now()}`);
    const col = await apiCreateColumn(request, boardA.name, 'Col');
    await apiCreateColumn(request, boardB.name, 'Col B');
    const card = await apiCreateCard(request, col.id, '# Version one');

    const eventsReady = page.waitForResponse(
      response =>
        response.request().method() === 'GET' &&
        response.url().includes(`/api/events?board_id=${boardA.id}`) &&
        response.ok()
    );
    await gotoBoardView(page, boardA.name);
    await eventsReady;
    const shown = page.locator('.card-item').first();
    await expect(shown).toContainText('Version one');

    // The last event the view hears for board A.
    await apiUpdateCard(request, card.id, { body: '# Version two' });
    await expect(shown).toContainText('Version two', { timeout: 5000 });

    // Away to B without a reload. B broadcasts nothing, so the view still
    // holds A's `CardUpdated` for version two.
    await openChooser(page);
    await page.locator('.chooser-board-row').filter({ hasText: boardB.name }).click();
    await expect(page.locator('.navbar-board-btn')).toContainText(boardB.name);

    // Changed while this tab is on B and hears nothing from A.
    await apiUpdateCard(request, card.id, { body: '# Version three' });

    // Back to A without a reload: the snapshot has version three.
    await openChooser(page);
    await page.locator('.chooser-board-row').filter({ hasText: boardA.name }).click();
    await expect(page.locator('.navbar-board-btn')).toContainText(boardA.name);
    await expect(shown).toContainText('Version three', { timeout: 5000 });
    // …and keeps it: a replayed stale event would land right after the
    // snapshot, so give it the chance.
    await page.waitForTimeout(1000);
    await expect(shown).toContainText('Version three');
    await expect(shown).not.toContainText('Version two');
  });
});

// Card #449. Behind a Traefik edge (HTTP/2 to the browser) the board missed
// events that arrived right after it loaded. Cause: events racing the
// snapshots they apply to, fixed by card #452's gates (this spec fails every
// run on the pre-#452 image). The other suspect — several events in one chunk
// overwriting each other in the board's single `sse_event` signal — was ruled
// out: Chromium runs a microtask checkpoint after every message event of a
// coalesced chunk, so the effects run per event. That browser property is
// pinned by its own spec below. This spec does not force or detect
// coalescing; it drives a burst of mutations fired the moment the stream
// answers and requires every one of them to land on top of a card snapshot
// held back until after the burst (#452's column-level card gate, forced
// every run). The board-level column and link gates are not forced here: a
// column's cards are only fetched once the columns snapshot has rendered it,
// so the rename below is ordinary traffic by then. Those gates are covered by
// the frontend's `land_columns` / `land_links` tests.
test.describe('a burst of events right after load', () => {
  test('every event of a burst fired as the stream opens is applied', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `sse-burst-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Burst');
    const EXISTING = 5;
    const existing = [];
    for (let i = 0; i < EXISTING; i++) {
      existing.push(await apiCreateCard(request, col.id, `# Existing ${i}`));
    }

    const panics: string[] = [];
    page.on('console', msg => {
      if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
    });
    page.on('pageerror', err => panics.push(String(err)));

    // Force the race rather than hope for it: the column's card snapshot is
    // read from the server at once, but handed to the page only after the
    // whole burst has happened. The page therefore always holds a snapshot
    // older than the events it has already been sent, which is exactly the
    // case #452's gate must reconcile (without it, the snapshot would
    // overwrite the edits and drop the creates).
    let snapshotRead!: () => void;
    const snapshotReadP = new Promise<void>(resolve => (snapshotRead = resolve));
    let releaseSnapshot!: () => void;
    const released = new Promise<void>(resolve => (releaseSnapshot = resolve));
    // Counts the board's non-audit events as the page receives them (an extra
    // listener; the app's own `onmessage` is untouched), so the snapshot is
    // released only once the page has been sent the whole burst.
    await page.addInitScript(() => {
      const Native = window.EventSource;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (window as any).__boardEvents = 0;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (window as any).EventSource = function (...args: unknown[]) {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const es = new (Native as any)(...args);
        es.addEventListener('message', (m: MessageEvent) => {
          try {
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            if (JSON.parse(m.data).type !== 'audit_appended') (window as any).__boardEvents++;
          } catch {
            /* not a board event */
          }
        });
        return es;
      };
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (window as any).EventSource.prototype = Native.prototype;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      Object.assign((window as any).EventSource, { CONNECTING: 0, OPEN: 1, CLOSED: 2 });
    });
    await page.route(`**/api/columns/${col.id}/cards`, async route => {
      if (route.request().method() !== 'GET') return route.fallback();
      const response = await route.fetch();
      snapshotRead();
      await released;
      await route.fulfill({ response });
    });

    const eventsReady = page.waitForResponse(
      response =>
        response.request().method() === 'GET' &&
        response.url().includes(`/api/events?board_id=${board.id}`) &&
        response.ok()
    );
    // Not `gotoBoardView`: the burst must not wait for the columns to render.
    await page.goto(`/boards/${board.name}`);
    const events = await eventsReady;
    await snapshotReadP;

    // The rig's point: the stream crossed the Traefik edge (its marker
    // header, see e2e/edge/dynamic.yml) over HTTP/2 to the browser.
    expect(await events.headerValue('x-e2e-edge')).toBe('traefik');
    expect(
      await page.evaluate(
        () => (performance.getEntriesByType('navigation')[0] as PerformanceNavigationTiming).nextHopProtocol
      )
    ).toBe('h2');

    // Card and column events back to back, interleaved with the audit event
    // every mutation also broadcasts. The audit events are not asserted: the
    // only reader is the history drawer, which ignores them while closed, and
    // it stays closed here. What must land is every card and column change
    // (the rename is not held by any gate; see above). The edits and the
    // rename go
    // concurrently; the creates are sequential (every create bumps the global
    // `card_counter` and computes a top-of-column position, and concurrent
    // creates fail with a 500 — card #456; that holds across boards, so this
    // also relies on the suite's single worker), but run alongside the edits
    // without waiting on the page.
    const CREATED = 5;
    await Promise.all([
      ...existing.map((card, i) => apiUpdateCard(request, card.id, { body: `# Edited ${i}` })),
      request
        .put(`/api/columns/${col.id}`, { data: { name: 'Burst renamed' } })
        .then(res => expect(res.ok()).toBe(true)),
      (async () => {
        for (let i = 0; i < CREATED; i++) await apiCreateCard(request, col.id, `# Created ${i}`);
      })(),
    ]);

    // Only once the page has been sent every card and column event of the
    // burst may the stale snapshot land, so they are all held behind it.
    const BURST_EVENTS = EXISTING + 1 + CREATED;
    await expect
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      .poll(() => page.evaluate(() => (window as any).__boardEvents as number), { timeout: 5000 })
      .toBeGreaterThanOrEqual(BURST_EVENTS);
    releaseSnapshot();

    const TOTAL = EXISTING + CREATED;
    const cards = page.locator('.card-item');
    await expect(cards).toHaveCount(TOTAL, { timeout: 5000 });
    for (let i = 0; i < EXISTING; i++) {
      await expect(cards.filter({ hasText: `Edited ${i}` })).toHaveCount(1);
    }
    for (let i = 0; i < CREATED; i++) {
      await expect(cards.filter({ hasText: `Created ${i}` })).toHaveCount(1);
    }
    await expect(page.locator('.column-name')).toHaveText('Burst renamed');
    await expect(page.locator('.card-count-badge').first()).toHaveText(String(TOTAL));

    // Still alive: the search repaints the list both ways.
    const search = page.locator('.navbar-search-input');
    await search.fill('Created 3');
    await expect(cards).toHaveCount(1);
    await search.fill('');
    await expect(cards).toHaveCount(TOTAL);
    expect(panics).toEqual([]);
  });
});

// Card #449. The board keeps the latest SSE event in one signal
// (`sse_event` in `board_view.rs`) and its effects read it from there, so it
// relies on the effects running between two message events — including two
// that reached the browser in the same network chunk, as an HTTP/2 edge may
// deliver them. Leptos runs effects in microtasks, so this holds exactly as
// long as the browser performs a microtask checkpoint after each message
// event. This pins that property on the suite's browser: a stream whose three
// events arrive in one response body must interleave each message with its
// own microtask. If it ever fails, the single signal can lose events and must
// become a queue.
test.describe('the browser the SSE plumbing relies on', () => {
  test('runs microtasks between the message events of one chunk', async ({ page }) => {
    // Both routes are served by Playwright, never by the app: the whole
    // stream body is delivered at once, which is the coalesced case.
    await page.route('**/__sse-probe/page', route =>
      route.fulfill({
        contentType: 'text/html',
        body: `<!doctype html><script>
          window.LOG = [];
          const es = new EventSource('/__sse-probe/stream');
          es.onmessage = m => {
            window.LOG.push('msg ' + m.data);
            queueMicrotask(() => window.LOG.push('micro ' + m.data));
            // A task queued at the first message: it can only run after the
            // other two if all three were dispatched without another task
            // in between — the precondition that they arrived together.
            if (m.data === 'a') {
              const ch = new MessageChannel();
              ch.port1.onmessage = () => window.LOG.push('task');
              ch.port2.postMessage(0);
            }
          };
        </script>`,
      })
    );
    await page.route('**/__sse-probe/stream', route =>
      route.fulfill({
        contentType: 'text/event-stream',
        body: 'data: a\n\ndata: b\n\ndata: c\n\n',
      })
    );
    await page.goto('/__sse-probe/page');
    // The stream ends after one body and EventSource reconnects, so only the
    // first delivery's seven entries are compared.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const read = () => page.evaluate(() => (window as any).LOG as string[]);
    await expect.poll(async () => (await read()).length).toBeGreaterThanOrEqual(7);
    const log = (await read()).slice(0, 7);
    expect(log).toEqual(['msg a', 'micro a', 'msg b', 'micro b', 'msg c', 'micro c', 'task']);
  });
});
