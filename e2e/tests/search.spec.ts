import { test, expect } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateColumn,
  apiCreateCard,
  apiUpdateCard,
  gotoBoardView,
} from './helpers';

test.describe('simple search', () => {
  test('filters by card number, card body, fuzzy query, and clear', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `search-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const target = await apiCreateCard(request, col.id, 'SSE card created in another context');
    await apiCreateCard(request, col.id, 'Deploy checklist');
    await apiCreateCard(request, col.id, 'Release notes');

    await gotoBoardView(page, board.name);
    const search = page.locator('.navbar-search-input');

    await search.fill(`#${target.number}`);
    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-item')).toContainText('SSE card created');

    await search.fill('deploy');
    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-item')).toContainText('Deploy checklist');

    await search.fill('sse crd');
    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-item')).toContainText('SSE card created');

    await search.fill('');
    await expect(page.locator('.card-item')).toHaveCount(3);
  });

  test('is scoped to the current board', async ({ page, request }) => {
    const boardA = await apiCreateBoard(request, `search-scope-a-${Date.now()}`);
    const boardB = await apiCreateBoard(request, `search-scope-b-${Date.now()}`);
    const colA = await apiCreateColumn(request, boardA.name, 'Todo');
    const colB = await apiCreateColumn(request, boardB.name, 'Todo');
    await apiCreateCard(request, colA.id, 'Scoped needle');
    await apiCreateCard(request, colB.id, 'Other board only');

    await gotoBoardView(page, boardA.name);
    await page.locator('.navbar-search-input').fill('scoped needle');

    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-item')).toContainText('Scoped needle');
    await expect(page.locator('.card-item')).not.toContainText('Other board only');
  });

  test('matching cards created over SSE appear while search is active', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `search-sse-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, 'Initial non-match');

    const context = await browser.newContext();
    const page = await context.newPage();
    await gotoBoardView(page, board.name);
    await page.locator('.navbar-search-input').fill('remote match');
    await expect(page.locator('.card-item')).toHaveCount(0);

    await apiCreateCard(request, col.id, 'Remote match created elsewhere');
    await expect(page.locator('.card-item')).toHaveCount(1, { timeout: 5000 });
    await expect(page.locator('.card-item')).toContainText('Remote match created elsewhere');

    await context.close();
  });

  test('cards updated over SSE appear when they start matching the active search', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `search-sse-update-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, 'Initial non-match');

    const context = await browser.newContext();
    const page = await context.newPage();
    await gotoBoardView(page, board.name);
    await page.locator('.navbar-search-input').fill('updated match');
    await expect(page.locator('.card-item')).toHaveCount(0);

    await apiUpdateCard(request, card.id, { body: 'Updated match from another context' });
    await expect(page.locator('.card-item')).toHaveCount(1, { timeout: 5000 });
    await expect(page.locator('.card-item')).toContainText('Updated match from another context');

    await context.close();
  });
});
