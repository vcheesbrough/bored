import { test, expect } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView } from './helpers';

test.describe('Cards', () => {
  test('create card via the + button', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Card Create Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');
    await gotoBoardView(page, board.id);

    // Click the add-card button; a new card is created immediately and opens in edit mode.
    await page.locator('.add-card-btn').first().click();

    // A card item should appear and be in expanded/editing state.
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();
  });

  test('edit card body and see markdown preview update', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Card Edit Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');
    await apiCreateCard(request, col.id, '');
    await gotoBoardView(page, board.id);

    // Expand the card by clicking it.
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();

    // Click the rendered body area to enter edit mode.
    await page.locator('.card-body-rendered').first().click();
    await expect(page.locator('.card-body-textarea').first()).toBeVisible();

    // Type markdown content.
    const body = `**Hello** from test ${Date.now()}`;
    await page.locator('.card-body-textarea').first().fill(body);

    // Blur to trigger auto-save and switch back to rendered view.
    await page.locator('.card-body-textarea').first().press('Escape');

    // The rendered preview should show the content.
    await expect(page.locator('.card-markdown').first()).toBeVisible();
  });

  test('open card in maximised modal', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Card Modal Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');
    const card = await apiCreateCard(request, col.id, 'Modal test card');
    await gotoBoardView(page, board.id);

    // Expand the card first.
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();

    // Click the maximise button (🗖).
    await page.locator('[title="Maximise"]').first().click();

    // Modal backdrop should appear.
    await expect(page.locator('.modal-backdrop')).toBeVisible();
    await expect(page.locator('.modal-card-number')).toBeVisible();

    // Close modal using the restore button (🗗).
    await page.locator('[title="Restore to board"]').click();
    await expect(page.locator('.modal-backdrop')).not.toBeVisible();
  });

  test('delete card via confirm modal', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Card Delete Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');
    const card = await apiCreateCard(request, col.id, 'Delete me');
    await gotoBoardView(page, board.id);

    // Expand the card.
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();

    // Click the delete button (✕).
    await page.locator('.card-toolbar-close').first().click();

    // Custom confirm dialog should appear.
    await expect(page.locator('.confirm-dialog')).toBeVisible();
    await expect(page.getByText('Delete this card?')).toBeVisible();

    // Confirm the deletion.
    await page.locator('.btn-danger').click();

    // Card should be gone.
    await expect(page.locator('.card-item')).toHaveCount(0);
  });

  test('cancel card deletion keeps the card', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Card Cancel Delete Board ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Column');
    await apiCreateCard(request, col.id, 'Keep me');
    await gotoBoardView(page, board.id);

    // Expand the card.
    await page.locator('.card-item').first().click();

    // Click delete and then cancel.
    await page.locator('.card-toolbar-close').first().click();
    await expect(page.locator('.confirm-dialog')).toBeVisible();
    await page.locator('.btn-ghost').click();

    // Card should still be there.
    await expect(page.locator('.card-item')).toHaveCount(1);
  });
});
