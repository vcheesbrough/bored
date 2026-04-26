import { test, expect } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, gotoBoardView, openChooser, closeChooser } from './helpers';

test.describe('Columns', () => {
  test('create column via board chooser', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `col-create-board-${Date.now()}`);
    await gotoBoardView(page, board.name);

    await openChooser(page);

    // Click "+ Add column" phantom row.
    await page.locator('.chooser-col-row-phantom').click();
    const name = `New Column ${Date.now()}`;
    await page.locator('.chooser-col-edit').fill(name);
    await page.locator('.chooser-col-edit').press('Enter');

    await closeChooser(page);

    // The new column should appear in the board view.
    await expect(page.locator('.column-name').filter({ hasText: name })).toBeVisible();
  });

  test('rename column via board chooser', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `col-rename-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, `Original Name ${Date.now()}`);
    await gotoBoardView(page, board.name);

    await openChooser(page);

    // Click the column name to start inline edit.
    await page
      .locator('.chooser-col-row')
      .filter({ hasText: col.name })
      .locator('.chooser-col-name')
      .click();

    const newName = `Renamed ${Date.now()}`;
    const input = page.locator('.chooser-col-row').filter({ hasText: col.name }).locator('.chooser-col-edit');
    await input.fill(newName);
    await input.press('Enter');

    await closeChooser(page);

    // Board view should show the updated name.
    await expect(page.locator('.column-name').filter({ hasText: newName })).toBeVisible();
    await expect(page.locator('.column-name').filter({ hasText: col.name })).not.toBeVisible();
  });

  test('delete column via board chooser', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `col-delete-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, `Delete Me ${Date.now()}`);
    await gotoBoardView(page, board.name);

    await openChooser(page);

    // Accept the browser-native confirm dialog before triggering the click.
    page.once('dialog', dialog => dialog.accept());
    await page
      .locator('.chooser-col-row')
      .filter({ hasText: col.name })
      .locator('.chooser-col-delete')
      .click();

    await closeChooser(page);

    // Column should be gone from the board view.
    await expect(page.locator('.column-name').filter({ hasText: col.name })).not.toBeVisible();
  });
});
