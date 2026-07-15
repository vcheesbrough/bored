import { expect, test } from '@playwright/test';
import { apiCreateBoard, apiCreateCard, apiCreateColumn, gotoBoardView } from './helpers';

test.describe('Iteration 33 - minimise columns', () => {
  test('expanded columns can be dragged one position to the right', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `column-drag-right-${Date.now()}`);
    const first = await apiCreateColumn(request, board.name, 'First', 0);
    const second = await apiCreateColumn(request, board.name, 'Second', 1);
    const third = await apiCreateColumn(request, board.name, 'Third', 2);
    await gotoBoardView(page, board.name);

    const columns = page.locator('.columns-row .column-view');
    await page
      .locator(`[data-column-id="${first.id}"] .column-grip`)
      .dragTo(page.locator(`[data-column-id="${second.id}"] .card-list`));

    await expect(columns.nth(0)).toHaveAttribute('data-column-id', second.id);
    await expect(columns.nth(1)).toHaveAttribute('data-column-id', first.id);
    await expect(columns.nth(2)).toHaveAttribute('data-column-id', third.id);

    await page.reload();
    await expect(columns.nth(0)).toHaveAttribute('data-column-id', second.id);
    await expect(columns.nth(1)).toHaveAttribute('data-column-id', first.id);
  });

  test('dropping on the visible column ghost completes the reorder', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `column-ghost-drop-${Date.now()}`);
    const first = await apiCreateColumn(request, board.name, 'First', 0);
    const second = await apiCreateColumn(request, board.name, 'Second', 1);
    await gotoBoardView(page, board.name);

    const firstGrip = page.locator(`[data-column-id="${first.id}"] .column-grip`);
    const secondColumn = page.locator(`[data-column-id="${second.id}"]`);
    await firstGrip.dispatchEvent('dragstart');
    await secondColumn.dispatchEvent('dragover');

    const ghost = page.locator('.column-ghost');
    await expect(ghost).toBeVisible();
    await ghost.dispatchEvent('dragover');
    await ghost.dispatchEvent('drop');
    await firstGrip.dispatchEvent('dragend');

    const columns = page.locator('.columns-row .column-view');
    await expect(columns.nth(0)).toHaveAttribute('data-column-id', second.id);
    await expect(columns.nth(1)).toHaveAttribute('data-column-id', first.id);

    await page.reload();
    await expect(columns.nth(0)).toHaveAttribute('data-column-id', second.id);
    await expect(columns.nth(1)).toHaveAttribute('data-column-id', first.id);
  });

  test('repeated bidirectional column drags persist their final order', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `column-drag-repeat-${Date.now()}`);
    const first = await apiCreateColumn(request, board.name, 'First', 0);
    const second = await apiCreateColumn(request, board.name, 'Second', 1);
    const third = await apiCreateColumn(request, board.name, 'Third', 2);
    await gotoBoardView(page, board.name);

    const columns = page.locator('.columns-row .column-view');
    const expectOrder = async (ids: string[]) => {
      for (const [index, id] of ids.entries()) {
        await expect(columns.nth(index)).toHaveAttribute('data-column-id', id);
      }
    };

    await page
      .locator(`[data-column-id="${first.id}"] .column-grip`)
      .dragTo(page.locator(`[data-column-id="${second.id}"] .column-header`));
    await expectOrder([second.id, first.id, third.id]);

    await page
      .locator(`[data-column-id="${third.id}"] .column-grip`)
      .dragTo(page.locator(`[data-column-id="${first.id}"] .card-list`));
    await expectOrder([second.id, third.id, first.id]);

    await page
      .locator(`[data-column-id="${second.id}"] .column-grip`)
      .dragTo(page.locator(`[data-column-id="${third.id}"] .column-header`));
    await expectOrder([third.id, second.id, first.id]);

    await page
      .locator(`[data-column-id="${first.id}"] .column-grip`)
      .dragTo(page.locator(`[data-column-id="${third.id}"] .card-list`));
    await expectOrder([first.id, third.id, second.id]);

    await page.reload();
    await expectOrder([first.id, third.id, second.id]);
  });

  test('collapsed columns persist per board and remain valid drag targets', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `collapsed-columns-${Date.now()}`);
    const source = await apiCreateColumn(request, board.name, 'Source', 0);
    const target = await apiCreateColumn(request, board.name, 'Target', 1);
    const final = await apiCreateColumn(request, board.name, 'Final', 2);
    await apiCreateCard(request, source.id, '# Move me');
    await apiCreateCard(request, target.id, '# Already here');

    await gotoBoardView(page, board.name);

    const columns = page.locator('.columns-row .column-view');
    const sourceColumn = page.locator(`[data-column-id="${source.id}"]`);
    const targetColumn = page.locator(`[data-column-id="${target.id}"]`);

    await targetColumn.getByRole('button', { name: 'Collapse column' }).click();
    await expect(targetColumn).toHaveClass(/column-collapsed/);
    await expect(targetColumn).toHaveAttribute('data-collapsed', 'true');
    await expect(targetColumn.locator('.add-card-btn')).not.toBeVisible();
    await expect(targetColumn.locator('.card-item')).not.toBeVisible();
    await expect(targetColumn.locator('.collapsed-column-name')).toHaveCSS('writing-mode', 'vertical-rl');
    expect((await targetColumn.boundingBox())?.width).toBeLessThanOrEqual(50);

    await expect
      .poll(() =>
        page.evaluate(
          ({ boardId, columnId }) => {
            const stored = localStorage.getItem(`bored:collapsed-columns:${boardId}`);
            return stored ? (JSON.parse(stored) as string[]).includes(columnId) : false;
          },
          { boardId: board.id, columnId: target.id }
        )
      )
      .toBe(true);

    await sourceColumn
      .locator('.card-item')
      .dragTo(targetColumn.locator('.collapsed-column-drop-target'));
    await expect(sourceColumn.locator('.card-item')).toHaveCount(0);
    await expect(targetColumn.locator('.collapsed-card-count')).toHaveText('2');

    await targetColumn.getByRole('button', { name: 'Expand column' }).click();
    await expect(targetColumn).not.toHaveClass(/column-collapsed/);
    await expect(targetColumn.locator('.card-item')).toHaveCount(2);
    await expect(targetColumn).toContainText('Move me');
    await expect(targetColumn.locator('.add-card-btn')).toBeVisible();

    await targetColumn.getByRole('button', { name: 'Collapse column' }).click();
    await targetColumn.locator('.collapsed-column-grip').dragTo(sourceColumn);
    await expect(columns.nth(0)).toHaveAttribute('data-column-id', target.id);
    await expect(columns.nth(1)).toHaveAttribute('data-column-id', source.id);
    await expect(columns.nth(2)).toHaveAttribute('data-column-id', final.id);

    await page.reload();
    await expect(page.locator(`[data-column-id="${target.id}"]`)).toHaveAttribute(
      'data-collapsed',
      'true'
    );
    await expect(columns.nth(0)).toHaveAttribute('data-column-id', target.id);

    const otherBoard = await apiCreateBoard(request, `collapsed-columns-other-${Date.now()}`);
    const otherColumn = await apiCreateColumn(request, otherBoard.name, 'Target', 0);
    await gotoBoardView(page, otherBoard.name);
    await expect(page.locator(`[data-column-id="${otherColumn.id}"]`)).toHaveAttribute(
      'data-collapsed',
      'false'
    );
    await expect(page.locator(`[data-column-id="${otherColumn.id}"] .add-card-btn`)).toBeVisible();
  });
});
