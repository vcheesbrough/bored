import { test, expect } from '@playwright/test';
import {
  apiBoardHistory,
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiDeleteCard,
  apiMoveCard,
  apiRestoreAudit,
  gotoBoardView,
  openChooser,
} from './helpers';

test.describe('Audit history & restore', () => {
  test('navbar opens history drawer', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-drawer-board-${Date.now()}`);
    await apiCreateColumn(request, board.name, 'Column');
    await gotoBoardView(page, board.name);

    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();
    await expect(page.locator('.history-drawer-title')).toHaveText('Board history');
  });

  test('board history lists card create after adding a card', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-create-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Audit trail body');
    await gotoBoardView(page, board.name);

    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const row = page.locator(`.history-row[data-entity-id="${card.id}"]`);
    await expect(row).toBeVisible();
    await expect(row.locator('.history-badge-create')).toBeVisible();
  });

  test('restore deleted card from history drawer', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-restore-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Restore me via UI');
    await gotoBoardView(page, board.name);

    await apiDeleteCard(request, card.id);
    await expect(page.locator('.card-item')).toHaveCount(0);

    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const deleteRow = page
      .locator(`.history-row[data-entity-id="${card.id}"]`)
      .filter({ has: page.locator('.history-badge-delete') });
    await expect(deleteRow).toBeVisible();
    await deleteRow.getByRole('button', { name: 'Restore' }).click();

    await expect(page.locator('.card-item')).toHaveCount(1);
    // Drawer + backdrop stay open and block clicks on the board until closed.
    await page.locator('.history-drawer-close').click();
    await expect(page.locator('.history-drawer')).not.toBeVisible();

    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-markdown').first()).toContainText('Restore me via UI');
  });

  test('column history drawer requests GET /api/columns/:id/history', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-col-endpoint-${Date.now()}`);
    const colTodo = await apiCreateColumn(request, board.name, 'Todo', 0);
    await apiCreateColumn(request, board.name, 'Other', 1);
    await apiCreateCard(request, colTodo.id, 'Card in Todo');
    await gotoBoardView(page, board.name);

    const historyRespPromise = page.waitForResponse(
      (res) =>
        res.request().method() === 'GET' &&
        res.url().includes(`/api/columns/${colTodo.id}/history`)
    );

    await openChooser(page);
    await page
      .locator('.chooser-col-row')
      .filter({ hasText: 'Todo' })
      .locator('[title="Column history"]')
      .click();

    const historyResp = await historyRespPromise;
    expect(historyResp.ok()).toBeTruthy();
    await expect(page.locator('.history-drawer-title')).toHaveText('Column history');
  });

  test('card history drawer requests GET /api/cards/:id/history', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-card-endpoint-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Scoped API');
    await gotoBoardView(page, board.name);

    const historyRespPromise = page.waitForResponse(
      (res) =>
        res.request().method() === 'GET' &&
        res.url().includes(`/api/cards/${card.id}/history`)
    );

    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-float-panel')).toBeVisible();
    await page.locator('.card-float-panel [title="Card history"]').click();

    const historyResp = await historyRespPromise;
    expect(historyResp.ok()).toBeTruthy();
    await expect(page.locator('.history-drawer-title')).toHaveText('Card history');
  });

  test('board chooser opens column-scoped history', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-col-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, 'Card A');
    await gotoBoardView(page, board.name);

    await openChooser(page);
    await page
      .locator('.chooser-col-row')
      .filter({ hasText: 'Todo' })
      .locator('[title="Column history"]')
      .click();
    await expect(page.locator('.history-drawer')).toBeVisible();
    await expect(page.locator('.history-drawer-title')).toHaveText('Column history');

    const cardRow = page.locator(`.history-row[data-entity-id="${card.id}"]`);
    await expect(cardRow).toBeVisible();
    await expect(cardRow.locator('.history-badge-create')).toBeVisible();
  });

  test('expanded card toolbar opens card-scoped history', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-inline-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Inline history');
    await gotoBoardView(page, board.name);

    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-float-panel')).toBeVisible();

    await page.locator('.card-float-panel [title="Card history"]').click();
    await expect(page.locator('.history-drawer')).toBeVisible();
    await expect(page.locator('.history-drawer-title')).toHaveText('Card history');

    const cardRow = page.locator(`.history-row[data-entity-id="${card.id}"]`);
    await expect(cardRow).toBeVisible();
  });

  test('card modal toolbar opens card-scoped history', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-modal-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Modal history');
    await gotoBoardView(page, board.name);

    await page.locator('.card-item').first().click();
    await page.locator('[title="Maximise"]').first().click();
    await expect(page.locator('.modal-backdrop')).toBeVisible();

    await page.locator('.modal-toolbar [title="Card history"]').click();
    await expect(page.locator('.history-drawer')).toBeVisible();
    await expect(page.locator('.history-drawer-title')).toHaveText('Card history');

    const cardRow = page.locator(`.history-row[data-entity-id="${card.id}"]`);
    await expect(cardRow).toBeVisible();

    await page.locator('.history-drawer-close').click();
    await expect(page.locator('.history-drawer')).not.toBeVisible();

    await page.locator('[title="Restore to board"]').click();
    await expect(page.locator('.modal-backdrop')).not.toBeVisible();
  });

  test('Show moves toggle hides and reveals move audit rows', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-moves-board-${Date.now()}`);
    const colA = await apiCreateColumn(request, board.name, 'A');
    const colB = await apiCreateColumn(request, board.name, 'B');
    const card = await apiCreateCard(request, colA.id, 'Movable');
    await apiMoveCard(request, card.id, colB.id, 0);

    await gotoBoardView(page, board.name);
    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    await expect(page.locator('.history-badge-move')).toHaveCount(0);

    await page.locator('.history-toggle input[type="checkbox"]').check();
    await expect(page.locator('.history-badge-move').first()).toBeVisible();
  });

  test('API restore matches UI restore outcome', async ({ request }) => {
    const board = await apiCreateBoard(request, `audit-api-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'API restore body');

    await apiDeleteCard(request, card.id);
    const hist = await apiBoardHistory(request, board.name);
    const deleteRow = hist.find((e) => e.action === 'delete' && e.entity_id === card.id);
    expect(deleteRow).toBeTruthy();

    await apiRestoreAudit(request, deleteRow!.id);

    const boardHistAfter = await apiBoardHistory(request, board.name);
    expect(boardHistAfter.some((e) => e.action === 'restore' && e.entity_type === 'card')).toBeTruthy();

    const cardRes = await request.get(`/api/cards/${card.id}`);
    expect(cardRes.ok()).toBeTruthy();
    const body = (await cardRes.json()) as { body: string };
    expect(body.body).toContain('API restore body');
  });
});
