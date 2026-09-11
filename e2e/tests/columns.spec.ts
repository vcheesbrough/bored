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
    await page.locator('.chooser-col-new').fill(name);
    await page.locator('.chooser-col-new').press('Enter');

    await closeChooser(page);

    // The new column should appear in the board view (scope to the kanban row — avoids strict-mode collisions).
    await expect(page.locator('.columns-row .column-name').filter({ hasText: name })).toBeVisible();
  });

  // Regression test for card #80. The create response and the SSE
  // `ColumnCreated` broadcast both deliver the new column, and the broadcast
  // normally arrives first, so an unguarded insert put the same column in the
  // list twice. Two entries sharing an id gave the keyed `<For>` duplicate keys,
  // after which it repeated an already-drawn name and dropped every column
  // created afterwards until the page was reloaded.
  test('create several columns in a row, each appearing exactly once', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `col-multi-board-${Date.now()}`);
    await apiCreateColumn(request, board.name, 'Existing A', 0);
    await apiCreateColumn(request, board.name, 'Existing B', 1);
    await gotoBoardView(page, board.name);

    await openChooser(page);
    await page.locator('.chooser-col-row-phantom').click();

    const newColInput = page.locator('.chooser-col-new');
    const boardColumn = (name: string) =>
      page.locator('.columns-row .column-name').filter({ hasText: name });

    await newColInput.pressSequentially('Alpha');
    await newColInput.press('Enter');
    await expect(boardColumn('Alpha')).toHaveCount(1);

    // The input stays open and focused so the next name can be typed straight
    // away. Hiding a focused input dropped focus to `<body>`, and everything
    // typed after that was silently discarded — no column, no error.
    await expect(newColInput).toBeFocused();
    await expect(newColInput).toHaveValue('');

    await page.keyboard.type('Beta');
    await page.keyboard.press('Enter');
    await expect(boardColumn('Beta')).toHaveCount(1);
    await expect(newColInput).toBeFocused();

    await page.keyboard.type('Gamma');
    await page.keyboard.press('Enter');
    await expect(boardColumn('Gamma')).toHaveCount(1);

    // No reload anywhere above: all three are rendered, once each, alongside
    // the originals, and every column has its own "Add card" button.
    await expect(boardColumn('Alpha')).toHaveCount(1);
    await expect(page.locator('.columns-row .column-name')).toHaveCount(5);
    await expect(page.locator('.columns-row [title="Add card"]')).toHaveCount(5);

    // Positions must be distinct and ascending, so a reload has to reproduce
    // the same order the client showed rather than resorting the board.
    const orderBeforeReload = await page.locator('.columns-row .column-name').allInnerTexts();
    await page.reload();
    await page.waitForSelector('.columns-row .column-name');
    await expect(page.locator('.columns-row .column-name')).toHaveCount(5);
    expect(await page.locator('.columns-row .column-name').allInnerTexts()).toEqual(orderBeforeReload);
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
    await expect(page.locator('.columns-row .column-name').filter({ hasText: newName })).toBeVisible();
    await expect(page.locator('.columns-row .column-name').filter({ hasText: col.name })).not.toBeVisible();
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
    await expect(page.locator('.columns-row .column-name').filter({ hasText: col.name })).not.toBeVisible();
  });
});
