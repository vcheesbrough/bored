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

  test('clear button appears only with a query and resets the search', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `search-clear-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, 'Deploy checklist');
    await apiCreateCard(request, col.id, 'Release notes');

    await gotoBoardView(page, board.name);
    const search = page.locator('.navbar-search-input');
    const clear = page.locator('.navbar-search-clear');

    // Hidden while the box is empty.
    await expect(clear).toHaveCount(0);

    await search.fill('deploy');
    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(clear).toBeVisible();

    await clear.click();
    // Clearing empties the query, restores every card, refocuses the input, and
    // hides the button again.
    await expect(search).toHaveValue('');
    await expect(page.locator('.card-item')).toHaveCount(2);
    await expect(search).toBeFocused();
    await expect(clear).toHaveCount(0);
  });

  test('Enter focuses the search box so searching is mouse-free', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `search-enter-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, 'Deploy checklist');
    await apiCreateCard(request, col.id, 'Release notes');

    await gotoBoardView(page, board.name);
    await expect(page.locator('.card-item')).toHaveCount(2);

    // With nothing interactive focused, Enter jumps into the search box; typing
    // then filters without ever touching the mouse.
    await page.keyboard.press('Enter');
    await expect(page.locator('.navbar-search-input')).toBeFocused();

    await page.keyboard.type('deploy');
    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-item')).toContainText('Deploy checklist');
  });

  test('Escape clears the query while the search box is focused', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `search-escape-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, 'Deploy checklist');
    await apiCreateCard(request, col.id, 'Release notes');

    await gotoBoardView(page, board.name);
    const search = page.locator('.navbar-search-input');

    await search.fill('deploy');
    await expect(page.locator('.card-item')).toHaveCount(1);

    await search.press('Escape');
    await expect(search).toHaveValue('');
    await expect(page.locator('.card-item')).toHaveCount(2);
  });

  test('hash-prefixed numbers match only the card number', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `search-number-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const target = await apiCreateCard(request, col.id, 'The actual numbered card');
    await apiCreateCard(request, col.id, `Body mentions #${target.number} but is not that card`);

    await gotoBoardView(page, board.name);
    await page.locator('.navbar-search-input').fill(`#${target.number}`);

    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-item')).toContainText('The actual numbered card');
    await expect(page.locator('.card-item')).not.toContainText('Body mentions');
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

  test('highlights matches in the card body', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `search-highlight-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, 'Deploy the deploy script');
    await apiCreateCard(request, col.id, 'Release notes');

    await gotoBoardView(page, board.name);
    const search = page.locator('.navbar-search-input');
    const marks = page.locator('.card-preview mark.search-hit');

    // Nothing is marked before a query is typed.
    await expect(marks).toHaveCount(0);

    await search.fill('deploy');
    await expect(page.locator('.card-item')).toHaveCount(1);
    // Both occurrences are marked, with the original casing preserved.
    await expect(marks).toHaveCount(2);
    await expect(marks.first()).toHaveText('Deploy');
    await expect(marks.nth(1)).toHaveText('deploy');
    // Yellow ground, card surface as the ink.
    await expect(marks.first()).toHaveCSS('background-color', 'rgb(251, 191, 36)');
    await expect(marks.first()).toHaveCSS('color', 'rgb(0, 56, 120)');

    // Clearing the query removes every mark.
    await search.fill('');
    await expect(page.locator('.card-item')).toHaveCount(2);
    await expect(marks).toHaveCount(0);
  });

  test('highlights the whole word for a fuzzy match', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `search-highlight-fuzzy-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, 'SSE card in another browser');

    await gotoBoardView(page, board.name);
    await page.locator('.navbar-search-input').fill('brwsr');

    const marks = page.locator('.card-preview mark.search-hit');
    await expect(page.locator('.card-item')).toHaveCount(1);
    // "brwsr" only matches as a subsequence, so the whole word lights up.
    await expect(marks).toHaveCount(1);
    await expect(marks.first()).toHaveText('browser');
  });

  test('a #number query highlights the badge, not the body', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `search-highlight-number-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const target = await apiCreateCard(request, col.id, 'Numbered card body text');

    await gotoBoardView(page, board.name);
    await page.locator('.navbar-search-input').fill(`#${target.number}`);

    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-preview mark.search-hit')).toHaveCount(0);
    const badge = page.locator('.card-item .card-number');
    await expect(badge).toHaveClass(/card-number-hit/);
    await expect(badge).toHaveCSS('background-color', 'rgb(251, 191, 36)');
  });

  test('highlights persist in the expanded card and the maximised modal', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `search-highlight-modal-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, '# Deploy plan\n\nRun the deploy step twice.');

    await gotoBoardView(page, board.name);
    await page.locator('.navbar-search-input').fill('deploy');
    await expect(page.locator('.card-item')).toHaveCount(1);

    // Expanding renders the full markdown — headings included — still marked.
    await page.locator('.card-item').click();
    const expandedMarks = page.locator('.card-markdown mark.search-hit');
    await expect(expandedMarks).toHaveCount(2);
    await expect(page.locator('.card-markdown h1 mark.search-hit')).toHaveText('Deploy');

    // Maximising carries the highlight into the modal.
    await page.locator('.card-toolbar-btn[title="Maximise"]').click();
    const modalMarks = page.locator('.modal-markdown mark.search-hit');
    await expect(modalMarks).toHaveCount(2);
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
    const eventsReady = page.waitForResponse(
      response =>
        response.request().method() === 'GET' &&
        response.url().includes(`/api/events?board_id=${board.id}`) &&
        response.ok()
    );
    await gotoBoardView(page, board.name);
    await eventsReady;
    await page.locator('.navbar-search-input').fill('updated match');
    await expect(page.locator('.card-item')).toHaveCount(0);

    await apiUpdateCard(request, card.id, { body: 'Updated match from another context' });
    await expect(page.locator('.card-item')).toHaveCount(1, { timeout: 5000 });
    await expect(page.locator('.card-item')).toContainText('Updated match from another context');

    await context.close();
  });
});
