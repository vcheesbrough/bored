import { test, expect, Browser } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView } from './helpers';

// All SSE tests use two independent browser contexts (A and B) connected to the
// same board. Context A performs a mutation; context B must reflect it without
// any manual reload.

test.describe('SSE real-time updates', () => {
  test('card created in context A appears in context B', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `SSE Create Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.id);

    // Context A clicks the + button to create a card.
    await pageA.locator('.add-card-btn').first().click();
    await expect(pageA.locator('.card-item')).toHaveCount(1);

    // Context B should receive the SSE event and show the new card.
    await expect(pageB.locator('.card-item')).toHaveCount(1, { timeout: 5000 });

    await ctxA.close();
    await ctxB.close();
  });

  test('card body edited in context A updates in context B', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `SSE Edit Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');
    await apiCreateCard(request, col.id, 'Original body');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.id);

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
    const board = await apiCreateBoard(request, `SSE Delete Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');
    await apiCreateCard(request, col.id, 'Goodbye');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.id);

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
    const board = await apiCreateBoard(request, `SSE Move Board ${Date.now()}`);
    const col1 = await apiCreateColumn(request, board.id, 'Source', 0);
    const col2 = await apiCreateColumn(request, board.id, 'Target', 1);
    await apiCreateCard(request, col1.id, 'Moving card');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.id);

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
    const board = await apiCreateBoard(request, `SSE Col Create Board ${Date.now()}`);

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.id);

    // Create a column via the API (which the backend broadcasts via SSE).
    await apiCreateColumn(request, board.id, 'New Column');

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
    const board = await apiCreateBoard(request, `SSE Col Rename Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Original Name');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.id);

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
    const board = await apiCreateBoard(request, `SSE Col Delete Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Delete Column');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.id);

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
  boardId: string
) {
  const pageA = await ctxA.newPage();
  const pageB = await ctxB.newPage();
  await gotoBoardView(pageA, boardId);
  await gotoBoardView(pageB, boardId);
  return [pageA, pageB] as const;
}
