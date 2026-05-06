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
} from './helpers';

test.describe('Audit history & restore', () => {
  test('navbar opens history drawer', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-drawer-board-${Date.now()}`);
    await apiCreateColumn(request, board.name, 'Column');
    await gotoBoardView(page, board.name);

    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();
    await expect(page.locator('.history-drawer-title')).toContainText('History');
  });

  test('board history lists card create after adding a card', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-create-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Audit trail body');
    await gotoBoardView(page, board.name);

    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const row = page.locator('.history-row').filter({ hasText: card.id });
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

    const deleteRow = page.locator('.history-row').filter({ hasText: card.id }).filter({
      has: page.locator('.history-badge-delete'),
    });
    await expect(deleteRow).toBeVisible();
    await deleteRow.getByRole('button', { name: 'Restore' }).click();

    await expect(page.locator('.card-item')).toHaveCount(1);
    // Drawer + backdrop stay open and block clicks on the board until closed.
    await page.locator('.history-drawer-close').click();
    await expect(page.locator('.history-drawer')).not.toBeVisible();

    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-markdown').first()).toContainText('Restore me via UI');
  });

  test('column header opens history scoped to column tab', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-col-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, 'Card A');
    await gotoBoardView(page, board.name);

    await page.locator('.column-history-btn').first().click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const columnTab = page.locator('.history-tab').nth(1);
    await expect(columnTab).toHaveClass(/history-tab-active/);
  });

  test('card modal toolbar opens card-scoped history tab', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-modal-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Modal history');
    await gotoBoardView(page, board.name);

    await page.locator('.card-item').first().click();
    await page.locator('[title="Maximise"]').first().click();
    await expect(page.locator('.modal-backdrop')).toBeVisible();

    await page.locator('[title="Card history"]').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const cardTab = page.locator('.history-tab').nth(2);
    await expect(cardTab).toHaveClass(/history-tab-active/);

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
