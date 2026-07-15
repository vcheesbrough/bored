import { test, expect } from '@playwright/test';
import {
  apiBoardHistory,
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiUpdateCard,
  gotoBoardView,
} from './helpers';

test.describe('Iteration 32 - card body versions', () => {
  test('previews and restores an earlier body with live updates', async ({
    page,
    request,
    context,
  }) => {
    const board = await apiCreateBoard(request, `body-versions-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Versions');
    const card = await apiCreateCard(
      request,
      column.id,
      '# Version A\n\nOriginal **markdown** body.'
    );
    await apiUpdateCard(request, card.id, {
      body: '# Version B\n\nCurrent body before restore.',
    });

    const beforeRestore = await apiBoardHistory(request, board.name);
    const versionA = beforeRestore.find(
      entry =>
        entry.entity_type === 'card' &&
        entry.entity_id === card.id &&
        entry.action === 'create'
    );
    expect(versionA).toBeTruthy();

    await gotoBoardView(page, board.name);
    await expect(page.locator('.card-item').first()).toContainText('Version B');

    const historyPage = await context.newPage();
    await gotoBoardView(historyPage, board.name);
    await historyPage.locator('.card-item').first().click();
    await historyPage.locator('.card-float-panel [title="Card history"]').click();
    await expect(historyPage.locator('.history-drawer')).toBeVisible();

    const currentRow = historyPage
      .locator(`.history-row[data-entity-id="${card.id}"]`)
      .filter({ has: historyPage.locator('.history-badge-update') });
    await expect(currentRow.locator('.history-current')).toHaveText('Current');
    await expect(currentRow.getByRole('button', { name: 'Restore version' })).toBeDisabled();

    const originalRow = historyPage
      .locator(`.history-row[data-entity-id="${card.id}"]`)
      .filter({ has: historyPage.locator('.history-badge-create') });
    await originalRow.getByRole('button', { name: 'Preview' }).click();
    const preview = originalRow.locator('.history-version-preview');
    await expect(preview).toBeVisible();
    await expect(preview.getByRole('heading', { name: 'Version A' })).toBeVisible();
    await expect(preview).toContainText('Original markdown body.');

    await originalRow.getByRole('button', { name: 'Restore version' }).click();

    await expect(page.locator('.card-item').first()).toContainText('Version A');
    const restoreRow = historyPage
      .locator(`.history-row[data-entity-id="${card.id}"]`)
      .filter({ has: historyPage.locator('.history-badge-restore') })
      .first();
    await expect(restoreRow.locator('.history-current')).toHaveText('Current');
    await expect(originalRow.getByRole('button', { name: 'Restore version' })).toBeDisabled();

    await expect
      .poll(async () => {
        const history = await apiBoardHistory(request, board.name);
        return history.find(
          entry =>
            entry.entity_type === 'card' &&
            entry.entity_id === card.id &&
            entry.action === 'restore'
        )?.restored_from;
      })
      .toBe(versionA!.id);

    await historyPage.close();
  });
});
