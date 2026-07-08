import { test, expect } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView } from './helpers';

test.describe('Cards', () => {
  test('create card via the + button', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-create-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await gotoBoardView(page, board.name);

    // Click the add-card button; a new card is created immediately and opens in edit mode.
    await page.locator('[title="Add card"]').first().click();

    // A card item should appear and be in expanded/editing state.
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();
  });

  test('edit card body and see markdown preview update', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-edit-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, '');
    await gotoBoardView(page, board.name);

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

  test('edit mode uses in-flow controls above a long first line', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-edit-layout-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(
      request,
      col.id,
      'This is a deliberately long first line that should not run underneath the edit toolbar buttons when the card is open for editing.'
    );
    await gotoBoardView(page, board.name);

    await page.locator('.card-item').first().click();
    await page.locator('.card-body-rendered').first().click();

    const toolbar = page.locator('.card-float-panel').first();
    const textarea = page.locator('.card-body-textarea').first();
    await expect(toolbar).toBeVisible();
    await expect(textarea).toBeVisible();

    const layout = await textarea.evaluate((el) => {
      const rect = el.getBoundingClientRect();
      const style = window.getComputedStyle(el);
      const toolbarStyle = window.getComputedStyle(
        document.querySelector('.card-float-panel')!
      );
      return {
        top: rect.top,
        paddingTop: parseFloat(style.paddingTop),
        borderTopWidth: parseFloat(style.borderTopWidth),
        toolbarPosition: toolbarStyle.position,
      };
    });
    const toolbarBox = await toolbar.boundingBox();

    expect(toolbarBox).not.toBeNull();
    expect(layout.toolbarPosition).toBe('static');
    expect(layout.top + layout.borderTopWidth + layout.paddingTop).toBeGreaterThanOrEqual(
      toolbarBox!.y + toolbarBox!.height
    );
  });

  test('markdown tables render in previews, expanded cards, and modal', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-table-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(
      request,
      col.id,
      '| Name | State |\n| --- | --- |\n| Search | Done |\n| Tables | Fixed |'
    );
    await gotoBoardView(page, board.name);

    const preview = page.locator('.card-preview').first();
    await expect(preview.locator('table')).toBeVisible();
    await expect(preview.locator('th')).toContainText(['Name', 'State']);
    await expect(preview.locator('td')).toContainText(['Search', 'Done', 'Tables', 'Fixed']);

    await page.locator('.card-item').first().click();
    const expanded = page.locator('.card-markdown').first();
    await expect(expanded.locator('table')).toBeVisible();
    await expect(expanded.locator('th')).toContainText(['Name', 'State']);
    await expect(expanded.locator('td')).toContainText(['Search', 'Done', 'Tables', 'Fixed']);

    await page.locator('[title="Maximise"]').first().click();
    const modal = page.locator('.modal-markdown');
    await expect(modal.locator('table')).toBeVisible();
    await expect(modal.locator('th')).toContainText(['Name', 'State']);
    await expect(modal.locator('td')).toContainText(['Search', 'Done', 'Tables', 'Fixed']);
  });

  test('open card in maximised modal', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-modal-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Modal test card');
    await gotoBoardView(page, board.name);

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
    const board = await apiCreateBoard(request, `card-delete-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Delete me');
    await gotoBoardView(page, board.name);

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
    const board = await apiCreateBoard(request, `card-cancel-delete-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Keep me');
    await gotoBoardView(page, board.name);

    // Expand the card.
    await page.locator('.card-item').first().click();

    // Click delete and then cancel.
    await page.locator('.card-toolbar-close').first().click();
    await expect(page.locator('.confirm-dialog')).toBeVisible();
    await page.locator('.btn-ghost').click();

    // Card should still be there.
    await expect(page.locator('.card-item')).toHaveCount(1);
  });

  test('card body persists after page reload', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-persist-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, '');
    await gotoBoardView(page, board.name);

    // Expand and edit the card body.
    await page.locator('.card-item').first().click();
    await page.locator('.card-body-rendered').first().click();
    const body = `Persistent content ${Date.now()}`;
    await page.locator('.card-body-textarea').first().fill(body);
    await page.locator('.card-body-textarea').first().press('Escape');

    // Reload and verify the content survived.
    await page.reload();
    await page.waitForSelector('.columns-row');
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-markdown').first()).toContainText('Persistent content');
  });

  test('edit card body in full-screen modal and verify save', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-modal-edit-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Original');
    await gotoBoardView(page, board.name);

    // Open the modal.
    await page.locator('.card-item').first().click();
    await page.locator('[title="Maximise"]').first().click();
    await expect(page.locator('.modal-backdrop')).toBeVisible();

    // Click the rendered body to enter edit mode, then fill the textarea.
    await page.locator('.modal-body-rendered').click();
    const newBody = `Modal edit ${Date.now()}`;
    await page.locator('.modal-body-textarea').fill(newBody);

    // Close with the restore button to return to the board.
    await page.locator('[title="Restore to board"]').click();
    await expect(page.locator('.modal-backdrop')).not.toBeVisible();

    // Reload and verify content persisted.
    await page.reload();
    await page.waitForSelector('.columns-row');
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-markdown').first()).toContainText('Modal edit');
  });

  test('Esc from editing returns to expanded, second Esc collapses card', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-esc-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Esc test');
    await gotoBoardView(page, board.name);

    // Click to expand, then click body to enter edit mode.
    await page.locator('.card-item').first().click();
    await page.locator('.card-body-rendered').first().click();
    await expect(page.locator('.card-body-textarea').first()).toBeVisible();

    // First Esc: editing → expanded (textarea hidden, rendered shown).
    await page.locator('.card-body-textarea').first().press('Escape');
    await expect(page.locator('.card-body-textarea').first()).not.toBeVisible();
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();

    // Second Esc: expanded → collapsed.
    await page.locator('.card-item').first().press('Escape');
    await expect(page.locator('.card-item.card-expanded')).not.toBeVisible();
  });

  test('Esc closes full-screen modal', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `card-modal-esc-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Modal Esc test');
    await gotoBoardView(page, board.name);

    // Open modal.
    await page.locator('.card-item').first().click();
    await page.locator('[title="Maximise"]').first().click();
    await expect(page.locator('.modal-backdrop')).toBeVisible();

    // Esc should close the modal.
    await page.keyboard.press('Escape');
    await expect(page.locator('.modal-backdrop')).not.toBeVisible();
  });
});
