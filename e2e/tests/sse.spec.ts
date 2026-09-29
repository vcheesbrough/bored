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
// coalesced chunk, so the effects run per event. This spec still drives that
// shape — a burst of mutations fired the moment the stream answers, while the
// column and card snapshots may still be in flight — and requires every one
// of them to land.
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

    const eventsReady = page.waitForResponse(
      response =>
        response.request().method() === 'GET' &&
        response.url().includes(`/api/events?board_id=${board.id}`) &&
        response.ok()
    );
    // Not `gotoBoardView`: the burst must not wait for the columns to render.
    await page.goto(`/boards/${board.name}`);
    await eventsReady;

    // Card, column and audit events interleaved, back to back, so the edge
    // is free to put several in one frame. The edits and the rename go
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
