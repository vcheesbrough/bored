import { test, expect } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView, openChooser } from './helpers';

// Board IDs use SurrealDB's format so URL matching needs a flexible pattern.
const BOARD_URL = /\/boards\/.+/;

test.describe('Boards', () => {
  test('home page redirects to the first existing board', async ({ page, request }) => {
    // Clear all boards so this board is guaranteed to be the redirect target.
    const res = await request.get('/api/boards');
    const boards = await res.json() as { id: string }[];
    for (const b of boards) await request.delete(`/api/boards/${b.id}`);

    const board = await apiCreateBoard(request, `Redirect Board ${Date.now()}`);
    await page.goto('/');
    await expect(page).toHaveURL(BOARD_URL);
    await expect(page.locator('.navbar-board-btn')).toContainText(board.name);
  });

  test('home page shows empty-state form when no boards exist', async ({ page, request }) => {
    // Delete all boards so the home page has nothing to redirect to.
    const res = await request.get('/api/boards');
    const boards = await res.json() as { id: string }[];
    for (const b of boards) {
      await request.delete(`/api/boards/${b.id}`);
    }

    await page.goto('/');
    await expect(page.locator('.empty-state')).toBeVisible();
    await expect(page.getByText('No boards yet')).toBeVisible();
  });

  test('create board from empty-state form and navigate to it', async ({ page, request }) => {
    // Ensure no boards exist.
    const res = await request.get('/api/boards');
    const boards = await res.json() as { id: string }[];
    for (const b of boards) {
      await request.delete(`/api/boards/${b.id}`);
    }

    await page.goto('/');
    await expect(page.locator('.empty-state')).toBeVisible();

    const name = `New Board ${Date.now()}`;
    await page.getByPlaceholder('Board name').fill(name);
    await page.getByRole('button', { name: 'Create board' }).click();

    await expect(page).toHaveURL(BOARD_URL);
    await expect(page.locator('.navbar-board-btn')).toContainText(name);
  });

  test('create board via board chooser', async ({ page, request }) => {
    const existing = await apiCreateBoard(request, `Anchor Board ${Date.now()}`);
    await gotoBoardView(page, existing.id);

    await openChooser(page);

    // Click "+ Add board" phantom row.
    await page.locator('.chooser-item-phantom').click();
    const name = `Chooser Board ${Date.now()}`;
    await page.locator('.chooser-item-input').fill(name);
    await page.locator('.chooser-item-input').press('Enter');

    // Should navigate to the new board.
    await expect(page).toHaveURL(BOARD_URL);
    await expect(page.locator('.navbar-board-btn')).toContainText(name);
  });

  test('switch to another board via board chooser', async ({ page, request }) => {
    const boardA = await apiCreateBoard(request, `Board A ${Date.now()}`);
    const boardB = await apiCreateBoard(request, `Board B ${Date.now()}`);
    await gotoBoardView(page, boardA.id);
    await expect(page.locator('.navbar-board-btn')).toContainText(boardA.name);

    await openChooser(page);
    // Click the board B row to navigate to it.
    await page.locator('.chooser-board-row').filter({ hasText: boardB.name }).click();

    await expect(page).toHaveURL(new RegExp(`/boards/${boardB.id}`));
    await expect(page.locator('.navbar-board-btn')).toContainText(boardB.name);
  });

  test('delete board with columns and cards navigates away', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Full Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');
    await apiCreateCard(request, col.id, 'A card');
    // Landing board so there is somewhere to go after deletion.
    await apiCreateBoard(request, `Landing Board ${Date.now()}`);

    await gotoBoardView(page, board.id);
    await expect(page.locator('.card-item')).toHaveCount(1);

    await openChooser(page);
    page.once('dialog', dialog => dialog.accept());
    await page
      .locator('.chooser-board-row')
      .filter({ hasText: board.name })
      .locator('.chooser-board-delete')
      .click();

    // App should leave the deleted board's URL.
    await expect(page).not.toHaveURL(`/boards/${board.id}`);
    await expect(page).toHaveURL(BOARD_URL);
  });

  test('delete board via board chooser', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Delete Board ${Date.now()}`);
    // Create a second board so there is somewhere to land after deletion.
    await apiCreateBoard(request, `Landing Board ${Date.now()}`);

    await gotoBoardView(page, board.id);
    await openChooser(page);

    // Accept the browser-native confirm dialog before triggering the click.
    page.once('dialog', dialog => dialog.accept());
    await page
      .locator('.chooser-board-row')
      .filter({ hasText: board.name })
      .locator('.chooser-board-delete')
      .click();

    // After deletion the app navigates away from the deleted board.
    await expect(page).not.toHaveURL(`/boards/${board.id}`);
  });
});
