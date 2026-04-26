import { test, expect } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView, openChooser } from './helpers';

// Board slugs are lowercase+digits+hyphens, so the URL pattern is the same.
const BOARD_URL = /\/boards\/.+/;

test.describe('Boards', () => {
  test('home page redirects to the first existing board', async ({ page, request }) => {
    // Clear all boards so this board is guaranteed to be the redirect target.
    const res = await request.get('/api/boards');
    const boards = await res.json() as { id: string; name: string }[];
    for (const b of boards) await request.delete(`/api/boards/${b.name}`);

    const board = await apiCreateBoard(request, `redirect-board-${Date.now()}`);
    await page.goto('/');
    await expect(page).toHaveURL(BOARD_URL);
    await expect(page.locator('.navbar-board-btn')).toContainText(board.name);
  });

  test('home page shows empty-state form when no boards exist', async ({ page, request }) => {
    // Delete all boards so the home page has nothing to redirect to.
    const res = await request.get('/api/boards');
    const boards = await res.json() as { id: string; name: string }[];
    for (const b of boards) {
      await request.delete(`/api/boards/${b.name}`);
    }

    await page.goto('/');
    await expect(page.locator('.empty-state')).toBeVisible();
    await expect(page.getByText('No boards yet')).toBeVisible();
  });

  test('create board from empty-state form and navigate to it', async ({ page, request }) => {
    // Ensure no boards exist.
    const res = await request.get('/api/boards');
    const boards = await res.json() as { id: string; name: string }[];
    for (const b of boards) {
      await request.delete(`/api/boards/${b.name}`);
    }

    await page.goto('/');
    await expect(page.locator('.empty-state')).toBeVisible();

    // Board names must be slug-format: lowercase letters, digits, hyphens.
    const name = `new-board-${Date.now()}`;
    await page.getByPlaceholder('Board name').fill(name);
    await page.getByRole('button', { name: 'Create board' }).click();

    await expect(page).toHaveURL(BOARD_URL);
    await expect(page.locator('.navbar-board-btn')).toContainText(name);
  });

  test('create board via board chooser', async ({ page, request }) => {
    const existing = await apiCreateBoard(request, `anchor-board-${Date.now()}`);
    await gotoBoardView(page, existing.name);

    await openChooser(page);

    // Click "+ Add board" phantom row.
    await page.locator('.chooser-item-phantom').click();
    const name = `chooser-board-${Date.now()}`;
    await page.locator('.chooser-item-input').fill(name);
    await page.locator('.chooser-item-input').press('Enter');

    // Should navigate to the new board.
    await expect(page).toHaveURL(BOARD_URL);
    await expect(page.locator('.navbar-board-btn')).toContainText(name);
  });

  test('switch to another board via board chooser', async ({ page, request }) => {
    const boardA = await apiCreateBoard(request, `board-a-${Date.now()}`);
    const boardB = await apiCreateBoard(request, `board-b-${Date.now()}`);
    await gotoBoardView(page, boardA.name);
    await expect(page.locator('.navbar-board-btn')).toContainText(boardA.name);

    await openChooser(page);
    // Click the board B row to navigate to it.
    await page.locator('.chooser-board-row').filter({ hasText: boardB.name }).click();

    await expect(page).toHaveURL(new RegExp(`/boards/${boardB.name}`));
    await expect(page.locator('.navbar-board-btn')).toContainText(boardB.name);
  });

  test('delete board with columns and cards navigates away', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `full-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'A card');
    // Landing board so there is somewhere to go after deletion.
    await apiCreateBoard(request, `landing-board-${Date.now()}`);

    await gotoBoardView(page, board.name);
    await expect(page.locator('.card-item')).toHaveCount(1);

    await openChooser(page);
    page.once('dialog', dialog => dialog.accept());
    await page
      .locator('.chooser-board-row')
      .filter({ hasText: board.name })
      .locator('.chooser-board-delete')
      .click();

    // App should leave the deleted board's URL.
    await expect(page).not.toHaveURL(`/boards/${board.name}`);
    await expect(page).toHaveURL(BOARD_URL);
  });

  test('delete board via board chooser', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `delete-board-${Date.now()}`);
    // Create a second board so there is somewhere to land after deletion.
    await apiCreateBoard(request, `landing-board-${Date.now()}`);

    await gotoBoardView(page, board.name);
    await openChooser(page);

    // Accept the browser-native confirm dialog before triggering the click.
    page.once('dialog', dialog => dialog.accept());
    await page
      .locator('.chooser-board-row')
      .filter({ hasText: board.name })
      .locator('.chooser-board-delete')
      .click();

    // After deletion the app navigates away from the deleted board.
    await expect(page).not.toHaveURL(`/boards/${board.name}`);
  });
});
