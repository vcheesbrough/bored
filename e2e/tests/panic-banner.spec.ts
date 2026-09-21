import { expect, test } from '@playwright/test';
import { apiCreateBoard, apiCreateCard, apiCreateColumn, gotoBoardView } from './helpers';

// Card #368: a WASM panic is fatal to the tab. Before this card it was also
// silent — the board just stopped responding. The panic hook now puts up a
// banner, outside the Leptos tree, with a way to reload.
//
// The panic is forced through the `bored:test-panic` window event, which the
// app listens for precisely so this can be tested against the real image.

/** Console lines from the panic hook (`wasm panic: …`). */
function watchPanics(page: import('@playwright/test').Page) {
  const panics: string[] = [];
  page.on('console', (msg) => {
    if (msg.text().includes('wasm panic')) panics.push(msg.text());
  });
  return panics;
}

test.describe('panic banner', () => {
  test('does not appear in a normal session', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `panic-none-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, column.id, 'Ordinary card');
    const panics = watchPanics(page);
    await gotoBoardView(page, board.name);

    // Do something real, so "no banner" is not just "nothing happened yet".
    const card = page.locator('.card-item', { hasText: 'Ordinary card' });
    await card.click();
    await expect(card).toHaveClass(/card-expanded/);

    await expect(page.locator('#panic-banner')).toHaveCount(0);
    expect(panics).toEqual([]);
  });

  test('appears on a panic, and Reload brings back a working board', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `panic-banner-${Date.now()}`);
    const column = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, column.id, 'Survivor');
    const panics = watchPanics(page);
    await gotoBoardView(page, board.name);
    await expect(page.locator('.card-item', { hasText: 'Survivor' })).toBeVisible();

    await page.evaluate(() => window.dispatchEvent(new Event('bored:test-panic')));

    const banner = page.locator('#panic-banner');
    await expect(banner).toBeVisible();
    await expect(banner).toHaveAttribute('role', 'alert');
    await expect(banner).toContainText('Reload to continue');
    // The console line the rest of the suite relies on is still written.
    expect(panics.some((p) => p.includes('deliberate panic'))).toBe(true);

    // A second panic does not stack a second banner.
    await page.evaluate(() => window.dispatchEvent(new Event('bored:test-panic')));
    await expect(page.locator('#panic-banner')).toHaveCount(1);

    // Reload is a plain link to this page — no Rust runs to follow it. Mark
    // this document first: the old page also has a `.columns-row`, so only the
    // mark's absence proves a fresh page load happened, rather than the router
    // swallowing the click and the old, dead page still being on screen.
    await page.evaluate(() => ((window as unknown as { __beforeReload: boolean }).__beforeReload = true));
    await banner.getByRole('link', { name: 'Reload' }).click();
    await expect
      .poll(() => page.evaluate(() => (window as unknown as { __beforeReload?: boolean }).__beforeReload ?? false))
      .toBe(false);
    await page.waitForSelector('.columns-row');
    await expect(page).toHaveURL(new RegExp(`/boards/${board.name}$`));
    await expect(page.locator('#panic-banner')).toHaveCount(0);

    // And the board is live again, not just painted: a card still expands.
    const card = page.locator('.card-item', { hasText: 'Survivor' });
    await card.click();
    await expect(card).toHaveClass(/card-expanded/);
  });
});
