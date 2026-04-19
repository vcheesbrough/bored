import { test, expect, Browser } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateColumn,
  apiCreateCard,
  apiDeleteCard,
  apiMoveCard,
  gotoBoardView,
} from './helpers';

test.describe('column card count badge', () => {
  test('empty column shows badge with 0', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Count Badge Empty ${Date.now()}`);
    await apiCreateColumn(request, board.id, 'Empty');
    await gotoBoardView(page, board.id);

    await expect(page.locator('.card-count-badge').first()).toHaveText('0');
  });

  test('badge increments when a card is added via API', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Count Badge Add ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Col');
    await gotoBoardView(page, board.id);

    await expect(page.locator('.card-count-badge').first()).toHaveText('0');

    await apiCreateCard(request, col.id, 'Card one');
    await expect(page.locator('.card-count-badge').first()).toHaveText('1', { timeout: 5000 });

    await apiCreateCard(request, col.id, 'Card two');
    await expect(page.locator('.card-count-badge').first()).toHaveText('2', { timeout: 5000 });
  });

  test('badge decrements when a card is deleted via API', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Count Badge Delete ${Date.now()}`);
    const col = await apiCreateColumn(request, board.id, 'Col');
    const card = await apiCreateCard(request, col.id, 'Bye');
    await gotoBoardView(page, board.id);

    await expect(page.locator('.card-count-badge').first()).toHaveText('1');

    await apiDeleteCard(request, card.id);
    await expect(page.locator('.card-count-badge').first()).toHaveText('0', { timeout: 5000 });
  });

  test('moving a card updates counts on both columns', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `Count Badge Move ${Date.now()}`);
    const src = await apiCreateColumn(request, board.id, 'Source', 0);
    const dst = await apiCreateColumn(request, board.id, 'Dest', 1);
    const card = await apiCreateCard(request, src.id, 'Traveller');
    await gotoBoardView(page, board.id);

    const badges = page.locator('.card-count-badge');
    await expect(badges.nth(0)).toHaveText('1');
    await expect(badges.nth(1)).toHaveText('0');

    await apiMoveCard(request, card.id, dst.id, 0);

    await expect(badges.nth(0)).toHaveText('0', { timeout: 5000 });
    await expect(badges.nth(1)).toHaveText('1', { timeout: 5000 });
  });

  test('badge updates in second browser context via SSE', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `Count Badge SSE ${Date.now()}`);
    await apiCreateColumn(request, board.id, 'Col');

    const [ctxA, ctxB] = await openTwoContexts(browser);
    const [pageA, pageB] = await openBoardInBoth(ctxA, ctxB, board.id);

    await expect(pageA.locator('.card-count-badge').first()).toHaveText('0');
    await expect(pageB.locator('.card-count-badge').first()).toHaveText('0');

    // Create a card in context A via the + button.
    await pageA.locator('.add-card-btn').first().click();
    await expect(pageA.locator('.card-count-badge').first()).toHaveText('1', { timeout: 5000 });

    // Context B must see the count update via SSE.
    await expect(pageB.locator('.card-count-badge').first()).toHaveText('1', { timeout: 5000 });

    await ctxA.close();
    await ctxB.close();
  });
});

// ── Helpers ───────────────────────────────────────────────────────────────

async function openTwoContexts(browser: Browser) {
  const baseURL = process.env.BASE_URL;
  const ctxA = await browser.newContext({ baseURL, ignoreHTTPSErrors: true });
  const ctxB = await browser.newContext({ baseURL, ignoreHTTPSErrors: true });
  return [ctxA, ctxB] as const;
}

async function openBoardInBoth(
  ctxA: Awaited<ReturnType<Browser['newContext']>>,
  ctxB: Awaited<ReturnType<Browser['newContext']>>,
  boardId: string
) {
  const pageA = await ctxA.newPage();
  const pageB = await ctxB.newPage();
  await gotoBoardView(pageA, boardId);
  await gotoBoardView(pageB, boardId);
  return [pageA, pageB] as const;
}
