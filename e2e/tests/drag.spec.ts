import { test, expect, Page } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView } from './helpers';

// Column grips use the HTML5 drag API (dragstart/dragover/drop) on a <span
// draggable="true">. Playwright's locator.dragTo() and mouse simulation don't
// fire dragstart on plain non-input elements. We dispatch the events directly.
async function dragColumnGrip(page: Page, fromIndex: number, toIndex: number) {
  // page.dragAndDrop uses CDP Input.dispatchDragEvent which fires trusted
  // drag events (isTrusted: true), unlike dispatchEvent() or mouse simulation.
  await page.dragAndDrop(
    `.column-grip >> nth=${fromIndex}`,
    `.column-view >> nth=${toIndex}`
  );
  // Allow the optimistic reorder and API call to settle.
  await page.waitForTimeout(500);
}

test.describe('Drag and drop', () => {
  test('move card to another column', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Drag Card Board ${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.id, 'Source', 0);
    const col2 = await apiCreateColumn(request, board.id, 'Target', 1);
    await apiCreateCard(request, col1.id, 'Drag me');
    await gotoBoardView(page, board.id);

    // Drag the card from column 1 to column 2's card-list.
    const card = page.locator('.column-view').nth(0).locator('.card-item').first();
    const targetList = page.locator('.column-view').nth(1).locator('.card-list');
    await card.dragTo(targetList);

    // Card should now be in column 2 and absent from column 1.
    await expect(page.locator('.column-view').nth(1).locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.column-view').nth(0).locator('.card-item')).toHaveCount(0);
  });

  test('moved card position persists after reload', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Drag Persist Board ${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.id, 'Source', 0);
    const col2 = await apiCreateColumn(request, board.id, 'Target', 1);
    const card = await apiCreateCard(request, col1.id, 'Persist me');
    await gotoBoardView(page, board.id);

    // Drag the card to col2.
    const cardEl = page.locator('.column-view').nth(0).locator('.card-item').first();
    const targetList = page.locator('.column-view').nth(1).locator('.card-list');
    await cardEl.dragTo(targetList);

    await expect(page.locator('.column-view').nth(1).locator('.card-item')).toHaveCount(1);

    // Reload and verify the card is still in col2.
    await page.reload();
    await page.waitForSelector('.columns-row');
    await expect(page.locator('.column-view').nth(1).locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.column-view').nth(0).locator('.card-item')).toHaveCount(0);
  });

  test('column order reflects API reorder (SSE update)', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Col Order Board ${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.id, 'Alpha', 0);
    const col2 = await apiCreateColumn(request, board.id, 'Beta', 1);
    await gotoBoardView(page, board.id);

    await expect(page.locator('.column-name').nth(0)).toHaveText('Alpha');
    await expect(page.locator('.column-name').nth(1)).toHaveText('Beta');

    // Reorder via API (same call the drag handler makes); SSE broadcasts the change.
    await request.put(`/api/boards/${board.id}/columns/reorder`, {
      data: { order: [col2.id, col1.id] },
    });

    // Board view should update without a reload.
    await expect(page.locator('.column-name').nth(0)).toHaveText('Beta', { timeout: 5000 });
    await expect(page.locator('.column-name').nth(1)).toHaveText('Alpha', { timeout: 5000 });
  });

  test('reorder cards within the same column via API and verify SSE update', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Card Reorder Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column', 0);
    // API inserts at top, so cardB (created second) appears above cardA.
    const cardA = await apiCreateCard(request, col.id, 'Card A');
    const cardB = await apiCreateCard(request, col.id, 'Card B');
    await gotoBoardView(page, board.id);

    // Card B is at index 0 (top), Card A at index 1.
    await expect(page.locator('.card-item').nth(0).locator('.card-preview')).toContainText('Card B');
    await expect(page.locator('.card-item').nth(1).locator('.card-preview')).toContainText('Card A');

    // Move Card B to the bottom by giving it a very high position value.
    await request.post(`/api/cards/${cardB.id}/move`, {
      data: { column_id: col.id, position: 999999 },
    });

    // The SSE event should flip the order in the live view.
    await expect(page.locator('.card-item').nth(0).locator('.card-preview')).toContainText('Card A', { timeout: 5000 });
    await expect(page.locator('.card-item').nth(1).locator('.card-preview')).toContainText('Card B', { timeout: 5000 });

    // Verify order survives reload.
    await page.reload();
    await page.waitForSelector('.columns-row');
    await expect(page.locator('.card-item').nth(0).locator('.card-preview')).toContainText('Card A');
    await expect(page.locator('.card-item').nth(1).locator('.card-preview')).toContainText('Card B');
  });

  test('column order persists after reload', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Col Persist Board ${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.id, 'First', 0);
    const col2 = await apiCreateColumn(request, board.id, 'Second', 1);
    await gotoBoardView(page, board.id);

    await request.put(`/api/boards/${board.id}/columns/reorder`, {
      data: { order: [col2.id, col1.id] },
    });

    await expect(page.locator('.column-name').nth(0)).toHaveText('Second', { timeout: 5000 });

    await page.reload();
    await page.waitForSelector('.columns-row');
    await expect(page.locator('.column-name').nth(0)).toHaveText('Second');
    await expect(page.locator('.column-name').nth(1)).toHaveText('First');
  });
});
