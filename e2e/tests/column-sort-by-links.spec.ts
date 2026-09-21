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

/**
 * The column's "Sort by links" button, once it is safe to press.
 *
 * The board fetches its links *after* its columns, so a column is painted and
 * its cards are listed a round trip before the link index exists. Clicking in
 * that window computes an order from no edges at all, which is a no-op — the
 * exact outcome the cross-column test asserts, so without this wait that test
 * could pass for entirely the wrong reason. The button is disabled until
 * `BoardLinkIndex::loaded` flips, so waiting for it to be enabled *is* the
 * signal that the links have landed.
 */
async function sortButton(column: ReturnType<typeof columnNamed>) {
  const button = column.getByRole('button', { name: 'Sort by links' });
  await expect(button).toBeEnabled();
  return button;
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

    await (await sortButton(column)).click();

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

    await (await sortButton(column)).click();

    // A real no-op, not a reorder that happened to land on the same order:
    // nothing was written, so no card was moved. The history check is the
    // decisive one — a late reorder would still show up as a move row.
    //
    // And a no-op for the right reason: `sortButton` waited for the link index,
    // so the cross-column edge was known to the browser and deliberately
    // dropped, rather than never having arrived.
    await expectCardOrder(column, ['Alpha', 'Beta']);
    const history = await apiBoardHistory(request, board.name);
    expect(history.filter((e) => e.entity_type === 'card' && e.action === 'move')).toEqual([]);
  });

  // ── Card #315: say what happened when nothing visibly did ─────────────────
  // All three of these used to leave the column untouched with only a console
  // line, so a failed save read as "already sorted".

  test('says so when the column is already in link order', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `sort-links-noop-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const beta = await apiCreateCard(request, col.id, '# Beta');
    const alpha = await apiCreateCard(request, col.id, '# Alpha');
    await apiCreateLink(request, alpha.id, 'successor', beta.id);

    await gotoBoardView(page, board.name);
    const column = columnNamed(page, 'Todo');
    await expectCardOrder(column, ['Alpha', 'Beta']);

    await (await sortButton(column)).click();

    // A status, not an alert: nothing went wrong.
    await expect(column.getByRole('status')).toHaveText('Already in link order.');
    await expect(column.getByRole('alert')).toHaveCount(0);

    // The claim is about the column as it was; once the column changes, it goes.
    await apiCreateCard(request, col.id, '# Gamma');
    await expectCardOrder(column, ['Gamma', 'Alpha', 'Beta']);
    await expect(column.getByRole('status')).toHaveCount(0);
  });

  test('shows an error when the new order cannot be saved, and clears it on the next click', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `sort-links-fail-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const alpha = await apiCreateCard(request, col.id, '# Alpha');
    const beta = await apiCreateCard(request, col.id, '# Beta');
    await apiCreateLink(request, alpha.id, 'successor', beta.id);

    // The server refuses the reorder once, then behaves.
    let refuse = true;
    await page.route('**/api/columns/*/cards/reorder', async (route) => {
      if (refuse) {
        refuse = false;
        await route.fulfill({ status: 500, body: 'boom' });
      } else {
        await route.continue();
      }
    });

    await gotoBoardView(page, board.name);
    const column = columnNamed(page, 'Todo');
    await expectCardOrder(column, ['Beta', 'Alpha']);

    await (await sortButton(column)).click();
    await expect(column.getByRole('alert')).toContainText("Couldn't save the new order");
    await expectCardOrder(column, ['Beta', 'Alpha']);

    // The retry succeeds, and the stale error does not outlive it.
    await (await sortButton(column)).click();
    await expectCardOrder(column, ['Alpha', 'Beta']);
    await expect(column.getByRole('alert')).toHaveCount(0);
  });

  test('shows a distinct error when the links form a loop', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `sort-links-loop-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const alpha = await apiCreateCard(request, col.id, '# Alpha');
    const beta = await apiCreateCard(request, col.id, '# Beta');
    await apiCreateLink(request, alpha.id, 'successor', beta.id);

    // The API refuses to close a loop, so one can only come from rows written
    // before that check existed. Stand one in by adding the reverse of the
    // real link to what the board's link fetch returns.
    await page.route(`**/api/boards/${board.name}/links`, async (route) => {
      const response = await route.fetch();
      const links = await response.json();
      const [link] = links;
      links.push({
        ...link,
        id: `${link.id}-reversed`,
        predecessor_id: link.successor_id,
        successor_id: link.predecessor_id,
        predecessor_number: link.successor_number,
        successor_number: link.predecessor_number,
      });
      await route.fulfill({ response, json: links });
    });

    await gotoBoardView(page, board.name);
    const column = columnNamed(page, 'Todo');
    await expectCardOrder(column, ['Beta', 'Alpha']);

    await (await sortButton(column)).click();
    await expect(column.getByRole('alert')).toHaveText(
      "Can't sort: the links between 2 cards form a loop."
    );
    // Nothing was sent, so nothing moved and nothing was recorded.
    await expectCardOrder(column, ['Beta', 'Alpha']);
    const history = await apiBoardHistory(request, board.name);
    expect(history.filter((e) => e.entity_type === 'card' && e.action === 'move')).toEqual([]);
  });
});
