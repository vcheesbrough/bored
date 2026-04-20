import { test, expect } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView } from './helpers';

// ── #77 No leading zeros on ticket numbers ────────────────────────────────

test.describe('Ticket number format', () => {
  test('card numbers render without leading zeros', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Number Format Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');
    // Create two cards; backend assigns sequential numbers (e.g. 1, 2).
    await apiCreateCard(request, col.id, 'First');
    await apiCreateCard(request, col.id, 'Second');
    await gotoBoardView(page, board.id);

    const numbers = await page.locator('.card-number').allTextContents();
    for (const n of numbers) {
      // Must match #<digits> with no leading zeros after the hash.
      expect(n).toMatch(/^#[1-9]\d*$/);
    }
  });
});

// ── #65 BUG: drag-over outline must clear after drop ─────────────────────

test.describe('Drag-over outline cleanup', () => {
  test('no drag-over outline remains after cross-column card drop', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Ghost Cleanup Board ${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.id, 'Source', 0);
    await apiCreateColumn(request, board.id, 'Target', 1);
    await apiCreateCard(request, col1.id, 'Drag me');
    await gotoBoardView(page, board.id);

    const card = page.locator('.column-view').nth(0).locator('.card-item').first();
    const targetList = page.locator('.column-view').nth(1).locator('.card-list');
    await card.dragTo(targetList);

    // After drop neither column should retain the drag-over outline.
    await expect(page.locator('.card-list.drag-over')).toHaveCount(0);
  });

  test('no drag-over outline remains when card dropped on another card', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Card-on-Card Drop Board ${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.id, 'Source', 0);
    const col2 = await apiCreateColumn(request, board.id, 'Target', 1);
    await apiCreateCard(request, col1.id, 'Drag me');
    await apiCreateCard(request, col2.id, 'Drop target');
    await gotoBoardView(page, board.id);

    // Drop the card from col1 directly on top of the card in col2.
    const dragCard = page.locator('.column-view').nth(0).locator('.card-item').first();
    const dropCard = page.locator('.column-view').nth(1).locator('.card-item').first();
    await dragCard.dragTo(dropCard);

    await expect(page.locator('.card-list.drag-over')).toHaveCount(0);
  });
});

// ── #68 Column drag ghost ─────────────────────────────────────────────────

test.describe('Column drag ghost', () => {
  test('column drag reorders columns and leaves no ghost after drop', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Col Ghost Board ${Date.now()}`);
    await apiCreateColumn(request, board.id, 'Alpha', 0);
    await apiCreateColumn(request, board.id, 'Beta', 1);
    await gotoBoardView(page, board.id);

    // Drag Beta (index 1) leftward onto Alpha (index 0) so the reorder produces
    // a visible DOM change [Alpha, Beta] → [Beta, Alpha].  The ghost placeholder
    // appears before Alpha during the drag to signal the insertion point.
    await page.locator('.column-grip').nth(1).dragTo(
      page.locator('.column-view').nth(0).locator('.card-list'),
    );

    // Column order must have flipped: drag_over_col_id / on_col_drop ran correctly.
    const names = await page.locator('.column-name').allTextContents();
    expect(names).toEqual(['Beta', 'Alpha']);

    // Ghost must be gone: drag_over_col_id was cleared by on_col_drop / dragend.
    await expect(page.locator('.column-ghost')).not.toBeVisible();
  });

  test('ghost is absent after each of several successive column drags', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Col Ghost Track Board ${Date.now()}`);
    await apiCreateColumn(request, board.id, 'One', 0);
    await apiCreateColumn(request, board.id, 'Two', 1);
    await apiCreateColumn(request, board.id, 'Three', 2);
    await gotoBoardView(page, board.id);

    // First drag: move col 1 (Two) before col 0 (One) → [Two, One, Three].
    await page.locator('.column-grip').nth(1).dragTo(
      page.locator('.column-view').nth(0).locator('.card-list'),
    );
    await expect(page.locator('.column-ghost')).not.toBeVisible();
    const order1 = await page.locator('.column-name').allTextContents();
    expect(order1).toEqual(['Two', 'One', 'Three']);

    // Second drag: move col 2 (Three) before col 0 (Two) → [Three, Two, One].
    await page.locator('.column-grip').nth(2).dragTo(
      page.locator('.column-view').nth(0).locator('.card-list'),
    );
    await expect(page.locator('.column-ghost')).not.toBeVisible();
    const order2 = await page.locator('.column-name').allTextContents();
    expect(order2).toEqual(['Three', 'Two', 'One']);
  });
});

// ── #24 Auto-reload on deployment (SSE reconnect with version change) ─────

test.describe('Auto-reload on deployment', () => {
  test('reloads when version changes after SSE reconnect', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Auto Reload Board ${Date.now()}`);

    // Inject a tracker to capture the EventSource instance before WASM runs.
    await page.addInitScript(() => {
      const OrigES = (window as any).EventSource;
      (window as any).__esInstances = [] as EventSource[];
      function PatchedES(this: EventSource, url: string, opts?: EventSourceInit) {
        const es = new OrigES(url, opts);
        (window as any).__esInstances.push(es);
        return es;
      }
      PatchedES.prototype = OrigES.prototype;
      PatchedES.CONNECTING = 0;
      PatchedES.OPEN = 1;
      PatchedES.CLOSED = 2;
      (window as any).EventSource = PatchedES;
    });

    await gotoBoardView(page, board.id);

    // Wait until the EventSource is OPEN (readyState 1), then give the
    // spawn_local fetch_app_info task a tick to store the baseline version.
    await page.waitForFunction(
      () => (window as any).__esInstances?.[0]?.readyState === 1,
      { timeout: 5000 },
    );
    await page.waitForTimeout(200);

    // From here, /api/info returns a different version (simulating a new deploy).
    await page.route('/api/info', (route) =>
      route.fulfill({ json: { version: '99.99.0', env: 'test' } }),
    );

    // Directly invoke the onerror + onopen handlers on the captured EventSource
    // to simulate a connection drop followed by a successful reconnect.
    const navigationPromise = page.waitForNavigation({ waitUntil: 'load', timeout: 8000 });
    await page.evaluate(() => {
      const es = (window as any).__esInstances?.[0] as any;
      if (!es) return;
      if (typeof es.onerror === 'function') es.onerror(new Event('error'));
      if (typeof es.onopen === 'function') es.onopen(new Event('open'));
    });

    // Page should have reloaded due to the version mismatch.
    await navigationPromise;

    // After reload the watermark should reflect the new version.
    await page.waitForSelector('.navbar-watermark');
    await expect(page.locator('.navbar-watermark')).toContainText('99.99.0', { timeout: 5000 });
  });

  test('does not reload when version is unchanged after reconnect', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `No Reload Board ${Date.now()}`);

    await page.addInitScript(() => {
      const OrigES = (window as any).EventSource;
      (window as any).__esInstances = [] as EventSource[];
      function PatchedES(this: EventSource, url: string, opts?: EventSourceInit) {
        const es = new OrigES(url, opts);
        (window as any).__esInstances.push(es);
        return es;
      }
      PatchedES.prototype = OrigES.prototype;
      PatchedES.CONNECTING = 0;
      PatchedES.OPEN = 1;
      PatchedES.CLOSED = 2;
      (window as any).EventSource = PatchedES;
    });

    await gotoBoardView(page, board.id);

    // Wait until the EventSource is OPEN, then give the baseline fetch a tick.
    await page.waitForFunction(
      () => (window as any).__esInstances?.[0]?.readyState === 1,
      { timeout: 5000 },
    );
    await page.waitForTimeout(200);

    // /api/info continues to return the SAME version — no reload expected.
    let navigated = false;
    page.once('load', () => { navigated = true; });

    await page.evaluate(() => {
      const es = (window as any).__esInstances?.[0] as any;
      if (!es) return;
      if (typeof es.onerror === 'function') es.onerror(new Event('error'));
      if (typeof es.onopen === 'function') es.onopen(new Event('open'));
    });

    // Wait long enough for any spurious reload to occur.
    await page.waitForTimeout(3000);
    expect(navigated).toBe(false);
  });
});

