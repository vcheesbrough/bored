import { expect, test } from '@playwright/test';
import { apiCreateBoard, apiCreateCard, apiCreateColumn, gotoBoardView } from './helpers';

async function openCardMenu(page: import('@playwright/test').Page, cardText: string) {
  await page.locator('.card-item', { hasText: cardText }).click({ button: 'right' });
  await expect(page.locator('.card-context-menu').first()).toBeVisible();
}

test.describe('Card context menu', () => {
  test('opens on right-click and dismisses without action', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-dismiss-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, column.id, 'Context target');
    await gotoBoardView(page, board.name);

    await openCardMenu(page, 'Context target');
    await page.keyboard.press('Escape');
    await expect(page.locator('.card-context-menu')).toHaveCount(0);

    await openCardMenu(page, 'Context target');
    await page.locator('.card-context-menu-backdrop').click({ button: 'right' });
    await expect(page.locator('.card-context-menu')).toHaveCount(0);

    await openCardMenu(page, 'Context target');
    await page.locator('.card-context-menu-backdrop').click({ position: { x: 4, y: 4 } });

    await expect(page.locator('.card-context-menu')).toHaveCount(0);
    await expect(page.locator('.card-item')).toHaveCount(1);
  });

  test('keeps the menu and submenu inside the viewport', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-bounds-${Date.now()}`);
    const source = await apiCreateColumn(request, board.name, 'Source', 0);
    await apiCreateColumn(request, board.name, 'Target', 1);
    await apiCreateCard(request, source.id, 'Viewport target');
    await gotoBoardView(page, board.name);

    const card = page.locator('.card-item', { hasText: 'Viewport target' });
    const viewport = page.viewportSize()!;
    await card.dispatchEvent('contextmenu', {
      clientX: viewport.width - 1,
      clientY: viewport.height - 1,
      button: 2,
    });
    await page.getByRole('menuitem', { name: 'Move to column' }).click();

    const menus = page.locator('.card-context-menu');
    await expect(menus).toHaveCount(2);
    for (const menu of await menus.all()) {
      const menuBox = await menu.boundingBox();
      expect(menuBox).not.toBeNull();
      expect(menuBox!.x).toBeGreaterThanOrEqual(0);
      expect(menuBox!.y).toBeGreaterThanOrEqual(0);
      expect(menuBox!.x + menuBox!.width).toBeLessThanOrEqual(viewport.width);
      expect(menuBox!.y + menuBox!.height).toBeLessThanOrEqual(viewport.height);
    }
  });

  test('moves a card to the top and bottom of its column', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-order-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, column.id, 'First');
    await apiCreateCard(request, column.id, 'Second');
    await apiCreateCard(request, column.id, 'Third');
    await gotoBoardView(page, board.name);

    // Cards are inserted at the top, so the initial order is Third, Second, First.
    await openCardMenu(page, 'First');
    await page.getByRole('menuitem', { name: 'Move to top' }).click();
    await expect(page.locator('.card-item').first()).toContainText('First');
    await expect(page.locator('.card-item', { hasText: 'First' })).not.toHaveClass(/card-expanded/);

    await openCardMenu(page, 'First');
    await page.getByRole('menuitem', { name: 'Move to bottom' }).click();
    await expect(page.locator('.card-item').last()).toContainText('First');

    await page.reload();
    await page.waitForSelector('.columns-row');
    await expect(page.locator('.card-item').last()).toContainText('First');
  });

  // Card #393. Repeated moves to the top use up the position gap above the
  // first card (~10 moves), and the server then renumbers the whole column.
  // Before the fix the renumbering was silent: this tab kept the siblings' old
  // positions, slotted the next "Move to top" by comparing against them, and
  // showed the card somewhere else until a reload. The report came in through
  // a search, so each move is made with a search active and checked after the
  // search is cleared.
  test('move to top stays on top across a column rebalance, through a search', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-rebalance-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    const names = ['Alpha', 'Bravo', 'Charlie'];
    for (const name of names) {
      await apiCreateCard(request, column.id, `${name} task`);
    }
    const panics: string[] = [];
    page.on('console', msg => {
      if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
    });
    page.on('pageerror', err => panics.push(String(err)));
    await gotoBoardView(page, board.name);

    const cards = page.locator('.card-item');
    const search = page.locator('.navbar-search-input');
    // The column top to bottom, as card names. `innerText` also carries the
    // card number and any badges, so match on the name rather than compare it.
    const order = async () =>
      (await cards.allInnerTexts()).map(t => names.find(n => t.includes(n)) ?? t);

    // 14 moves is comfortably past the ~10 that force a rebalance (pinned by
    // the backend's `bisecting_the_same_slot_survives_about_ten_inserts`), and
    // always moving the bottom card means every move changes the order.
    for (let i = 0; i < 14; i++) {
      const bottom = (await order()).at(-1)!;
      await search.fill('task');
      await expect(cards).toHaveCount(3);
      await openCardMenu(page, bottom);
      await page.getByRole('menuitem', { name: 'Move to top' }).click();
      await page.locator('.navbar-search-clear').click();
      await expect(cards.first(), `move ${i + 1}`).toContainText(bottom);
    }

    // What this tab shows is what the server stores.
    const shown = await order();
    await page.reload();
    await page.waitForSelector('.columns-row');
    await expect.poll(order).toEqual(shown);

    // Still responsive after all that: one more move lands.
    await openCardMenu(page, shown[0]);
    await page.getByRole('menuitem', { name: 'Move to bottom' }).click();
    await expect(cards.last()).toContainText(shown[0]);
    expect(panics).toEqual([]);
  });

  test('moves a card through the other-column submenu', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-columns-${Date.now()}`);
    const source = await apiCreateColumn(request, board.name, 'Source', 0);
    await apiCreateColumn(request, board.name, 'Target', 1);
    await apiCreateCard(request, source.id, 'Move between columns');
    await gotoBoardView(page, board.name);

    await openCardMenu(page, 'Move between columns');
    await page.getByRole('menuitem', { name: 'Move to column' }).click();
    await expect(page.locator('.card-context-menu-submenu')).toBeVisible();
    await expect(page.locator('.card-context-menu-submenu')).not.toContainText('Source');
    await page.locator('.card-context-menu-submenu').getByRole('menuitem', { name: 'Target' }).click();

    await expect(page.locator('.column-view').nth(0).locator('.card-item')).toHaveCount(0);
    await expect(page.locator('.column-view').nth(1).locator('.card-item')).toContainText('Move between columns');
  });

  test('uses the existing confirmation dialog before deleting', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-delete-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, column.id, 'Delete from menu');
    await gotoBoardView(page, board.name);

    await openCardMenu(page, 'Delete from menu');
    await page.getByRole('menuitem', { name: 'Delete' }).click();
    await expect(page.locator('.confirm-dialog')).toBeVisible();
    await page.locator('.btn-danger').click();

    await expect(page.locator('.card-item')).toHaveCount(0);
  });

  test('does not collapse a different expanded card when deleting from the menu', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `context-menu-preserve-expand-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, column.id, 'Keep open');
    await apiCreateCard(request, column.id, 'Delete me');
    await gotoBoardView(page, board.name);

    const openCard = page.locator('.card-item', { hasText: 'Keep open' });
    await openCard.click();
    await expect(openCard).toHaveClass(/card-expanded/);

    await openCardMenu(page, 'Delete me');
    await page.getByRole('menuitem', { name: 'Delete' }).click();
    await page.locator('.btn-danger').click();

    await expect(page.locator('.card-item', { hasText: 'Delete me' })).toHaveCount(0);
    await expect(openCard).toHaveClass(/card-expanded/);
  });
});
