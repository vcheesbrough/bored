import { expect, test, type Page } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiDeleteColumn,
  gotoBoardView,
  openChooser,
} from './helpers';

/** The collapsed-column ids stored for `boardId`, sorted; `null` when unset. */
async function storedCollapsed(page: Page, boardId: string): Promise<string[] | null> {
  return page.evaluate(id => {
    const stored = localStorage.getItem(`bored:collapsed-columns:${id}`);
    return stored ? (JSON.parse(stored) as string[]).sort() : null;
  }, boardId);
}

/** Collect panics and disposed-signal errors, which wedge the WASM tab silently. */
function watchForPanics(page: Page): string[] {
  const panics: string[] = [];
  page.on('console', msg => {
    if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
  });
  page.on('pageerror', err => panics.push(String(err)));
  return panics;
}

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

    await page
      .locator(`[data-column-id="${final.id}"] .column-grip`)
      .dragTo(targetColumn.locator('.collapsed-column-drop-target'));
    await expect(columns.nth(0)).toHaveAttribute('data-column-id', source.id);
    await expect(columns.nth(1)).toHaveAttribute('data-column-id', final.id);
    await expect(columns.nth(2)).toHaveAttribute('data-column-id', target.id);

    await page.reload();
    await expect(columns.nth(0)).toHaveAttribute('data-column-id', source.id);
    await expect(columns.nth(1)).toHaveAttribute('data-column-id', final.id);
    await expect(columns.nth(2)).toHaveAttribute('data-column-id', target.id);

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

  // Card #649: collapse state lives in a board-level context. `BoardView` is
  // reused across `/boards/:slug`, so switching boards in-app (the chooser, not
  // a full page load) must load the set of the board being shown — neither
  // keeping the previous board's set nor losing this one's. Both boards have a
  // collapsed column, so a set loaded only once per page (the bug a reused
  // component invites) shows up on the first switch.
  test('a collapsed column stays collapsed after reload and after switching boards in-app', async ({
    page,
    request,
  }) => {
    const panics = watchForPanics(page);
    const boardA = await apiCreateBoard(request, `collapse-ctx-a-${Date.now()}`);
    const folded = await apiCreateColumn(request, boardA.name, 'Folded', 0);
    const open = await apiCreateColumn(request, boardA.name, 'Open', 1);
    const boardB = await apiCreateBoard(request, `collapse-ctx-b-${Date.now()}`);
    const otherFolded = await apiCreateColumn(request, boardB.name, 'Other folded', 0);
    const otherOpen = await apiCreateColumn(request, boardB.name, 'Other open', 1);
    const foldedColumn = page.locator(`[data-column-id="${folded.id}"]`);
    const openColumn = page.locator(`[data-column-id="${open.id}"]`);
    const otherFoldedColumn = page.locator(`[data-column-id="${otherFolded.id}"]`);
    const otherOpenColumn = page.locator(`[data-column-id="${otherOpen.id}"]`);

    // Board B gets a collapsed column of its own first.
    await gotoBoardView(page, boardB.name);
    await otherFoldedColumn.getByRole('button', { name: 'Collapse column' }).click();
    await expect(otherFoldedColumn).toHaveAttribute('data-collapsed', 'true');
    await expect.poll(() => storedCollapsed(page, boardB.id)).toEqual([otherFolded.id]);

    // Then board A, by a full page load.
    await gotoBoardView(page, boardA.name);
    await foldedColumn.getByRole('button', { name: 'Collapse column' }).click();
    await expect(foldedColumn).toHaveAttribute('data-collapsed', 'true');
    await expect(openColumn).toHaveAttribute('data-collapsed', 'false');
    // Same key and format as before the lift, so existing users keep theirs.
    await expect.poll(() => storedCollapsed(page, boardA.id)).toEqual([folded.id]);

    await page.reload();
    await expect(foldedColumn).toHaveAttribute('data-collapsed', 'true');
    await expect(openColumn).toHaveAttribute('data-collapsed', 'false');

    // In-app switch to B: B's own set, and nothing of A's.
    await openChooser(page);
    await page.locator('.chooser-board-row').filter({ hasText: boardB.name }).click();
    await expect(page).toHaveURL(new RegExp(`/boards/${boardB.name}$`));
    await expect(otherFoldedColumn).toHaveAttribute('data-collapsed', 'true');
    await expect(otherFoldedColumn.locator('.add-card-btn')).not.toBeVisible();
    await expect(otherOpenColumn).toHaveAttribute('data-collapsed', 'false');
    await expect(otherOpenColumn.locator('.add-card-btn')).toBeVisible();

    // And back to A, still in-app: A's set is loaded again.
    await openChooser(page);
    await page.locator('.chooser-board-row').filter({ hasText: boardA.name }).click();
    await expect(page).toHaveURL(new RegExp(`/boards/${boardA.name}$`));
    await expect(foldedColumn).toHaveAttribute('data-collapsed', 'true');
    await expect(foldedColumn.locator('.add-card-btn')).not.toBeVisible();
    await expect(openColumn).toHaveAttribute('data-collapsed', 'false');

    // Liveness: the board still reacts after all that — expanding works, the
    // emptied set removes A's key rather than storing `[]`, and the write went
    // to A's key only.
    await foldedColumn.getByRole('button', { name: 'Expand column' }).click();
    await expect(foldedColumn).toHaveAttribute('data-collapsed', 'false');
    await expect(foldedColumn.locator('.add-card-btn')).toBeVisible();
    await expect.poll(() => storedCollapsed(page, boardA.id)).toBeNull();
    expect(await storedCollapsed(page, boardB.id)).toEqual([otherFolded.id]);
    expect(panics).toEqual([]);
  });

  test('deleting a collapsed column drops it from the stored set', async ({ page, request }) => {
    const panics = watchForPanics(page);
    const board = await apiCreateBoard(request, `collapse-ctx-delete-${Date.now()}`);
    const doomed = await apiCreateColumn(request, board.name, 'Doomed', 0);
    const kept = await apiCreateColumn(request, board.name, 'Kept', 1);
    await gotoBoardView(page, board.name);

    const doomedColumn = page.locator(`[data-column-id="${doomed.id}"]`);
    const keptColumn = page.locator(`[data-column-id="${kept.id}"]`);
    await doomedColumn.getByRole('button', { name: 'Collapse column' }).click();
    await keptColumn.getByRole('button', { name: 'Collapse column' }).click();
    await expect
      .poll(() => storedCollapsed(page, board.id))
      .toEqual([doomed.id, kept.id].sort());

    // Deleted elsewhere: this tab hears it over SSE (`ColumnDeleted`).
    await apiDeleteColumn(request, doomed.id);
    await expect(doomedColumn).toHaveCount(0);
    await expect.poll(() => storedCollapsed(page, board.id)).toEqual([kept.id]);

    // Liveness: the surviving column is still collapsed and still responds.
    await expect(keptColumn).toHaveAttribute('data-collapsed', 'true');
    await keptColumn.getByRole('button', { name: 'Expand column' }).click();
    await expect(keptColumn).toHaveAttribute('data-collapsed', 'false');
    await expect.poll(() => storedCollapsed(page, board.id)).toBeNull();
    expect(panics).toEqual([]);
  });
});
