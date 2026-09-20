import { test, expect } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView } from './helpers';

// ── #77 No leading zeros on ticket numbers ────────────────────────────────

test.describe('Ticket number format', () => {
  test('card numbers render without leading zeros', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `number-format-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    // Create two cards; backend assigns sequential numbers (e.g. 1, 2).
    await apiCreateCard(request, col.id, 'First');
    await apiCreateCard(request, col.id, 'Second');
    await gotoBoardView(page, board.name);

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
    const board = await apiCreateBoard(request, `ghost-cleanup-board-${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.name, 'Source', 0);
    await apiCreateColumn(request, board.name, 'Target', 1);
    await apiCreateCard(request, col1.id, 'Drag me');
    await gotoBoardView(page, board.name);

    const card = page.locator('.column-view').nth(0).locator('.card-item').first();
    const targetList = page.locator('.column-view').nth(1).locator('.card-list');
    await card.dragTo(targetList);

    // After drop neither column should retain the drag-over outline.
    await expect(page.locator('.card-list.drag-over')).toHaveCount(0);
  });

  test('no drag-over outline remains when card dropped on another card', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-on-card-drop-board-${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.name, 'Source', 0);
    const col2 = await apiCreateColumn(request, board.name, 'Target', 1);
    await apiCreateCard(request, col1.id, 'Drag me');
    await apiCreateCard(request, col2.id, 'Drop target');
    await gotoBoardView(page, board.name);

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
    const board = await apiCreateBoard(request, `col-ghost-board-${Date.now()}`);
    await apiCreateColumn(request, board.name, 'Alpha', 0);
    await apiCreateColumn(request, board.name, 'Beta', 1);
    await gotoBoardView(page, board.name);

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
    const board = await apiCreateBoard(request, `col-ghost-track-board-${Date.now()}`);
    await apiCreateColumn(request, board.name, 'One', 0);
    await apiCreateColumn(request, board.name, 'Two', 1);
    await apiCreateColumn(request, board.name, 'Three', 2);
    await gotoBoardView(page, board.name);

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

// The #24 auto-reload suite that used to live here moved to
// connection.spec.ts in iteration 58: the reload is no longer driven by the
// SSE reconnect it was written around, and the old specs only passed because
// they fired the EventSource handlers by hand.
