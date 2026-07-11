import { expect, test } from '@playwright/test';
import { apiCreateBoard, apiCreateCard, apiCreateColumn, gotoBoardView } from './helpers';

async function openCardMenu(page: import('@playwright/test').Page, cardText: string) {
  await page.locator('.card-item', { hasText: cardText }).click({ button: 'right' });
  await expect(page.locator('.card-context-menu').first()).toBeVisible();
}

test.describe('Card context menu', () => {
  test('opens on right-click and dismisses without action', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-dismiss-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, column.id, 'Context target');
    await gotoBoardView(page, board.name);

    await openCardMenu(page, 'Context target');
    await page.keyboard.press('Escape');
    await expect(page.locator('.card-context-menu')).toHaveCount(0);

    await openCardMenu(page, 'Context target');
    await page.locator('.card-context-menu-backdrop').click({ position: { x: 4, y: 4 } });

    await expect(page.locator('.card-context-menu')).toHaveCount(0);
    await expect(page.locator('.card-item')).toHaveCount(1);
  });

  test('moves a card to the top and bottom of its column', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-order-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, column.id, 'First');
    await apiCreateCard(request, column.id, 'Second');
    await apiCreateCard(request, column.id, 'Third');
    await gotoBoardView(page, board.name);

    // Cards are inserted at the top, so the initial order is Third, Second, First.
    await openCardMenu(page, 'First');
    await page.getByRole('menuitem', { name: 'Move to top' }).click();
    await expect(page.locator('.card-item').first()).toContainText('First');
    await expect(page.locator('.card-item', { hasText: 'First' })).not.toHaveClass(/card-expanded/);

    await openCardMenu(page, 'First');
    await page.getByRole('menuitem', { name: 'Move to bottom' }).click();
    await expect(page.locator('.card-item').last()).toContainText('First');

    await page.reload();
    await page.waitForSelector('.columns-row');
    await expect(page.locator('.card-item').last()).toContainText('First');
  });

  test('moves a card through the other-column submenu', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-columns-${Date.now()}`);
    const source = await apiCreateColumn(request, board.name, 'Source', 0);
    await apiCreateColumn(request, board.name, 'Target', 1);
    await apiCreateCard(request, source.id, 'Move between columns');
    await gotoBoardView(page, board.name);

    await openCardMenu(page, 'Move between columns');
    await page.getByRole('menuitem', { name: 'Move to column' }).click();
    await expect(page.locator('.card-context-menu-submenu')).toBeVisible();
    await expect(page.locator('.card-context-menu-submenu')).not.toContainText('Source');
    await page.locator('.card-context-menu-submenu').getByRole('menuitem', { name: 'Target' }).click();

    await expect(page.locator('.column-view').nth(0).locator('.card-item')).toHaveCount(0);
    await expect(page.locator('.column-view').nth(1).locator('.card-item')).toContainText('Move between columns');
  });

  test('uses the existing confirmation dialog before deleting', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-delete-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, column.id, 'Delete from menu');
    await gotoBoardView(page, board.name);

    await openCardMenu(page, 'Delete from menu');
    await page.getByRole('menuitem', { name: 'Delete' }).click();
    await expect(page.locator('.confirm-dialog')).toBeVisible();
    await page.locator('.btn-danger').click();

    await expect(page.locator('.card-item')).toHaveCount(0);
  });
});
