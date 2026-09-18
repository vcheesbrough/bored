import { test, expect } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  gotoBoardView,
} from './helpers';

// Most-recently-used ordering in the two suggestion combo boxes — iteration
// 48 / card #310.
//
// Both lists lead with what this browser picked before, in pick order, and
// fall back to how recently each card changed. The picks are kept in
// `localStorage` per board, so they survive a reload.

/** The expanded card whose body contains `text`. */
function cardWith(page: import('@playwright/test').Page, text: string) {
  return page.locator('.card-item').filter({ hasText: text });
}

/** Labels currently offered by the link picker, in order. */
async function pickerOptions(page: import('@playwright/test').Page) {
  return await page.locator('.link-suggestions .link-suggestion').allTextContents();
}

test.describe('most-recently-used suggestions', () => {
  test('the link picker leads with the card you linked last', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `mru-links-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    // Created oldest first, so `updated_at` ascends with the card number.
    const alpha = await apiCreateCard(request, col.id, '# Alpha card');
    const bravo = await apiCreateCard(request, col.id, '# Bravo card');
    const charlie = await apiCreateCard(request, col.id, '# Charlie card');
    const delta = await apiCreateCard(request, col.id, '# Delta card');

    await gotoBoardView(page, board.name);
    await cardWith(page, 'Alpha card').click();

    // Nothing picked yet: newest card first, which is the reverse of the
    // numeric order this list used to be in.
    const after = page.locator('.link-group[data-side="after"]');
    await after.locator('.link-picker-input').click();
    await expect
      .poll(() => pickerOptions(page))
      .toEqual([
        `#${delta.number} Delta card`,
        `#${charlie.number} Charlie card`,
        `#${bravo.number} Bravo card`,
      ]);

    // Link the *oldest* candidate, so pick order and recency disagree.
    await page
      .locator('.link-suggestions .link-suggestion')
      .filter({ hasText: 'Bravo card' })
      .click();
    await expect(after.locator('.link-chip-card')).toHaveText(`#${bravo.number} Bravo card`);

    // Collapse Alpha before moving on: while its picker is open the popup's
    // own rows put every other card's title inside Alpha's `.card-item`.
    await page.locator('.card-toolbar-btn[title="Collapse"]').click();

    // Another card's picker now offers Bravo first, ahead of the newer cards.
    await cardWith(page, 'Charlie card').click();
    const charlieAfter = page.locator('.link-group[data-side="after"]');
    await charlieAfter.locator('.link-picker-input').click();
    await expect
      .poll(() => pickerOptions(page))
      .toEqual([
        `#${bravo.number} Bravo card`,
        `#${delta.number} Delta card`,
        `#${alpha.number} Alpha card`,
      ]);

    // The pick is stored per board, so it outlives the page. (Reloading also
    // clears the open picker, so Delta's card is unambiguous again.)
    await gotoBoardView(page, board.name);
    await cardWith(page, 'Delta card').click();
    await page
      .locator('.link-group[data-side="after"] .link-picker-input')
      .click();
    await expect
      .poll(async () => (await pickerOptions(page))[0])
      .toBe(`#${bravo.number} Bravo card`);
  });

  test('the # popup leads with the tag you picked last', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `mru-tags-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, '# Bug card', ['bug']);
    await apiCreateCard(request, col.id, '# Chore card', ['chore']);
    await apiCreateCard(request, col.id, '# Docs card', ['docs']);

    await gotoBoardView(page, board.name);
    const search = page.locator('.navbar-search-input');
    const tagRows = page.locator('.search-suggestions .search-suggestion', {
      hasText: /^#[a-z]+$/,
    });

    // No picks yet: the tag on the newest card leads, not the alphabetical
    // first one.
    await search.fill('#');
    await expect(tagRows).toHaveText(['#docs', '#chore', '#bug']);

    // Pick the tag that recency puts last.
    await tagRows.filter({ hasText: '#bug' }).click();
    await expect(search).toHaveValue('#bug ');

    await search.fill('#');
    await expect(tagRows).toHaveText(['#bug', '#docs', '#chore']);

    // Reload: the pick is still remembered.
    await gotoBoardView(page, board.name);
    await page.locator('.navbar-search-input').fill('#');
    await expect(tagRows.first()).toHaveText('#bug');
  });
});
