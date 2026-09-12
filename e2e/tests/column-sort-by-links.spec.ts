import { test, expect } from '@playwright/test';
import {
  apiBoardHistory,
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiCreateLink,
  gotoBoardView,
} from './helpers';

// Sort a column by its card links — iteration 46 / card #311.
//
// The column header's "Sort by links" button re-orders that column so every
// card sits below its predecessors, moving as few cards as the links allow and
// ignoring links that point outside the column.
//
// `apiCreateCard` inserts at the *top* of the column, so creating Alpha, Beta,
// Gamma in that order leaves the column reading Gamma, Beta, Alpha.

/**
 * The column whose header is exactly `name`. Anchored because `hasText` is
 * substring matching by default, which would let a "Todo" lookup also match a
 * column called "Todo later"; case-insensitive because `.column-name` is
 * `text-transform: uppercase` and Playwright matches the rendered text, which
 * the string form of `hasText` would have ignored for free.
 */
function columnNamed(page: import('@playwright/test').Page, name: string) {
  return page
    .locator('.column-view')
    .filter({ has: page.locator('.column-name', { hasText: new RegExp(`^${name}$`, 'i') }) });
}

/** Collapsed-card preview text, top to bottom, in the given column. */
async function cardOrder(column: ReturnType<typeof columnNamed>): Promise<string[]> {
  return (await column.locator('.card-preview').allInnerTexts()).map((t) => t.trim());
}

/**
 * Wait for a column to read exactly `expected`, top to bottom. Always polled:
 * cards are fetched per column after the board view mounts, so a column is
 * briefly empty both on first load and after a reload.
 */
async function expectCardOrder(column: ReturnType<typeof columnNamed>, expected: string[]) {
  await expect.poll(() => cardOrder(column)).toEqual(expected);
}

test.describe('sort a column by card links', () => {
  test('puts a predecessor above its successor and leaves unlinked cards alone', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `sort-links-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const alpha = await apiCreateCard(request, col.id, '# Alpha');
    await apiCreateCard(request, col.id, '# Beta');
    const gamma = await apiCreateCard(request, col.id, '# Gamma');

    // Alpha comes before Gamma, but the column reads Gamma, Beta, Alpha.
    await apiCreateLink(request, alpha.id, 'successor', gamma.id);

    await gotoBoardView(page, board.name);
    const column = columnNamed(page, 'Todo');
    await expectCardOrder(column, ['Gamma', 'Beta', 'Alpha']);

    await column.getByRole('button', { name: 'Sort by links' }).click();

    // Beta is unlinked and was already between the two, so only Alpha and
    // Gamma swap around it.
    await expectCardOrder(column, ['Beta', 'Alpha', 'Gamma']);

    // Reload to read the order back from the database rather than from the
    // live view: an SSE-updated list can look right while the stored positions
    // are duplicated and the on-disk order is ambiguous.
    await page.reload();
    await page.waitForSelector('.columns-row');
    await expectCardOrder(columnNamed(page, 'Todo'), ['Beta', 'Alpha', 'Gamma']);
  });

  test('ignores a link to a card in another column', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `sort-links-other-${Date.now()}`);
    const todo = await apiCreateColumn(request, board.name, 'Todo');
    const doing = await apiCreateColumn(request, board.name, 'Doing');
    const beta = await apiCreateCard(request, todo.id, '# Beta');
    await apiCreateCard(request, todo.id, '# Alpha');
    const zulu = await apiCreateCard(request, doing.id, '# Zulu');

    // Zulu comes before Beta, but Zulu is in another column: the link says
    // nothing about where Beta belongs within Todo.
    await apiCreateLink(request, zulu.id, 'successor', beta.id);

    await gotoBoardView(page, board.name);
    const column = columnNamed(page, 'Todo');
    await expectCardOrder(column, ['Alpha', 'Beta']);

    await column.getByRole('button', { name: 'Sort by links' }).click();

    // A real no-op, not a reorder that happened to land on the same order:
    // nothing was written, so no card was moved. The history check is the
    // decisive one — a late reorder would still show up as a move row.
    await expectCardOrder(column, ['Alpha', 'Beta']);
    const history = await apiBoardHistory(request, board.name);
    expect(history.filter((e) => e.entity_type === 'card' && e.action === 'move')).toEqual([]);
  });
});
