import { test, expect } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateColumn,
  apiCreateCard,
  apiCreateLink,
  apiGetCard,
  apiUpdateCard,
  gotoBoardView,
} from './helpers';

/**
 * A trap surfaces as a `wasm panic:` console error or an `unreachable` page
 * error. One definition for the whole file: the describe blocks that assert
 * liveness (#375, #305) both watch for the same thing, and the regex should
 * only have to be corrected once if a future Leptos changes the message.
 */
function watchForPanics(page: import('@playwright/test').Page) {
  const panics: string[] = [];
  page.on('console', msg => {
    if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
  });
  page.on('pageerror', err => panics.push(String(err)));
  return panics;
}

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

  // ── Typing a search one keystroke at a time — card #375 ──────────────────
  //
  // The report: a card reading `abcdef` is still shown when the search says
  // `abcXXX`. Every other test in this file sets the query with one `fill`,
  // which only exercises "unfiltered → filtered"; typing is what reaches "was
  // matching a keystroke ago, is not now".
  //
  // For a *collapsed* card that transition was always right — the first test
  // pins it down so the report's literal scenario has a regression test. The
  // card that lingered was the *expanded* one: it is pinned into the filtered
  // view so that editing it out of the filter cannot unmount it mid-edit
  // (#304), and the pin held against the query changing too. A freshly created
  // card is auto-expanded, which is how the report was reached.
  test.describe('typing past a match', () => {
    /** Body text of every card on screen, in order — `<mark>`s contribute only their text. */
    const visibleBodies = (page: import('@playwright/test').Page) =>
      page.locator('.card-item .card-preview');

    test('collapsed cards leave on the keystroke that breaks their match', async ({
      page,
      request,
    }) => {
      const board = await apiCreateBoard(request, `search-keystroke-board-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      // New cards land at the top, so create in reverse of the order on screen.
      for (const body of ['abcdefxxx', 'apple', 'abq', 'abcxyz', 'zzz', 'abcdef']) {
        await apiCreateCard(request, col.id, body);
      }

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      const bodies = visibleBodies(page);
      await expect(bodies).toHaveText(['abcdef', 'zzz', 'abcxyz', 'abq', 'apple', 'abcdefxxx']);

      // The survivors after each keystroke, worked out by hand from the bodies:
      // a term matches a word it is a substring *or* an in-order subsequence of,
      // ignoring case. `toHaveText` with an array asserts the exact list, so a
      // card that fails to leave is as much a failure as one that leaves early.
      const steps: [string, string[]][] = [
        ['a', ['abcdef', 'abcxyz', 'abq', 'apple', 'abcdefxxx']],
        ['b', ['abcdef', 'abcxyz', 'abq', 'abcdefxxx']],
        ['c', ['abcdef', 'abcxyz', 'abcdefxxx']],
        // `abcx`: `abcdef` has no x — the report's card goes here.
        ['X', ['abcxyz', 'abcdefxxx']],
        // `abcxx`: `abcxyz` has only one x.
        ['X', ['abcdefxxx']],
        ['X', ['abcdefxxx']],
      ];
      await page.locator('.navbar-search-input').click();
      for (const [key, expected] of steps) {
        await page.keyboard.type(key);
        await expect(bodies).toHaveText(expected);
      }
      await expect(page.locator('.navbar-search-input')).toHaveValue('abcXXX');

      // Widening the search again brings cards back on the keystroke that
      // re-admits them, not only on a full clear.
      await page.keyboard.press('Backspace');
      await page.keyboard.press('Backspace');
      await expect(bodies).toHaveText(['abcxyz', 'abcdefxxx']);
      await page.keyboard.press('Backspace');
      await expect(bodies).toHaveText(['abcdef', 'abcxyz', 'abcdefxxx']);
      expect(panics).toEqual([]);
    });

    test('an expanded card leaves when the query moves past it, and stays while it matches', async ({
      page,
      request,
    }) => {
      const board = await apiCreateBoard(request, `search-keystroke-expanded-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      await apiCreateCard(request, col.id, 'Release notes');
      await apiCreateCard(request, col.id, 'abcdef');

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await expect(page.locator('.card-item')).toHaveCount(2);

      await page.locator('.card-item').filter({ hasText: 'abcdef' }).click();
      const expanded = page.locator('.card-item.card-expanded');
      await expect(expanded).toHaveCount(1);

      // Narrowing the search *around* the open card must not slam it shut.
      await page.locator('.navbar-search-input').click();
      await page.keyboard.type('abc');
      await expect(page.locator('.card-item')).toHaveCount(1);
      await expect(expanded).toHaveCount(1);
      await expect(expanded).toContainText('abcdef');

      // The keystroke that breaks the match releases it — expanded or not.
      await page.keyboard.type('X');
      await expect(page.locator('.card-item')).toHaveCount(0);
      await page.keyboard.type('XX');
      await expect(page.locator('.card-item')).toHaveCount(0);

      // Back within reach of the query it returns, collapsed: the search let
      // go of it, so nothing on the board is expanded any more.
      for (let i = 0; i < 3; i++) await page.keyboard.press('Backspace');
      await expect(page.locator('.card-item')).toHaveCount(1);
      await expect(page.locator('.card-item')).toContainText('abcdef');
      await expect(expanded).toHaveCount(0);

      // The lock really was released rather than left pointing at a hidden
      // card: another card can be expanded, and it is the only one that is.
      await page.locator('.navbar-search-input').fill('');
      await expect(page.locator('.card-item')).toHaveCount(2);
      await page.locator('.card-item').filter({ hasText: 'Release notes' }).click();
      await expect(expanded).toHaveCount(1);
      await expect(expanded).toContainText('Release notes');
      expect(panics).toEqual([]);
    });

    test('a freshly created card does not linger in a search typed after it', async ({
      page,
      request,
    }) => {
      // The report's own route. `+` creates the card expanded and in edit mode;
      // the search box is then clicked with the textarea still holding the text,
      // so this also proves the edit is saved on the way out rather than lost
      // with the card when the search unmounts it.
      const board = await apiCreateBoard(request, `search-keystroke-created-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      await apiCreateCard(request, col.id, 'Release notes');

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await expect(page.locator('.card-item')).toHaveCount(1);

      const created = page.waitForResponse(
        response =>
          response.request().method() === 'POST' &&
          response.url().includes(`/api/columns/${col.id}/cards`) &&
          response.ok()
      );
      await page.locator('.add-card-btn').click();
      const newCard = (await (await created).json()) as { id: string };
      const textarea = page.locator('.card-item.card-expanded textarea');
      await expect(textarea).toBeVisible();
      await textarea.fill('abcdef');

      // Two clicks, and the assertion between them and the typing, are
      // deliberate. Leaving the editor blurs the textarea, and the card answers
      // by focusing its own rendered body — taking focus back from the search
      // box that was just clicked. That is a separate defect (card #401), out of
      // scope for #375; without the second click the keystrokes land on the card
      // and the search box stays empty, failing this test for a reason that has
      // nothing to do with the pin. A second click is harmless once that is
      // fixed, so the test holds either way.
      const search = page.locator('.navbar-search-input');
      await search.click();
      // The editor closing is the card's reaction to the blur having finished,
      // so the second click cannot race the focus grab it is there to undo. The
      // textarea stays mounted — it shares a grid cell with the rendered body —
      // and is only hidden, hence the class rather than a count.
      await expect(textarea).toHaveClass(/card-body-hidden/);
      await search.click();
      await expect(search).toBeFocused();
      await page.keyboard.type('abcXXX');
      await expect(search).toHaveValue('abcXXX');

      await expect(page.locator('.card-item')).toHaveCount(0);
      await expect.poll(async () => (await apiGetCard(request, newCard.id)).body).toBe('abcdef');
      expect(panics).toEqual([]);

      // The board still reacts — a wedged executor would repaint nothing — and
      // the card comes back carrying the text that was typed into it.
      await page.locator('.navbar-search-input').fill('');
      await expect(visibleBodies(page)).toHaveText(['abcdef', 'Release notes']);
      expect(panics).toEqual([]);
    });
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

  // ── `#number` reaches one hop along the links — card #305 ───────────────
  // `#42` answers "what is going on around this ticket": card 42 plus the
  // cards linked directly before and after it. The cards pulled in by a link
  // carry neither the number nor the text, so the link badge that put them
  // there is highlighted instead of their own number badge.
  test.describe('#number includes linked cards', () => {
    test('shows direct links in both directions, and stops there', async ({ page, request }) => {
      const board = await apiCreateBoard(request, `search-link-board-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      const target = await apiCreateCard(request, col.id, 'The hub card');
      const before = await apiCreateCard(request, col.id, 'Comes first');
      const after = await apiCreateCard(request, col.id, 'Deploy the thing');
      const twoHops = await apiCreateCard(request, col.id, 'Two hops out');
      await apiCreateCard(request, col.id, 'Nothing to do with it');

      // before → target → after → twoHops.
      await apiCreateLink(request, target.id, 'predecessor', before.id);
      await apiCreateLink(request, target.id, 'successor', after.id);
      await apiCreateLink(request, after.id, 'successor', twoHops.id);

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await expect(page.locator('.card-item')).toHaveCount(5);

      const search = page.locator('.navbar-search-input');
      await search.fill(`#${target.number}`);

      // The hub and its two neighbours — not the card two hops out, and not
      // the unlinked one.
      const cards = page.locator('.card-item');
      await expect(cards).toHaveCount(3);
      await expect(cards.filter({ hasText: 'The hub card' })).toHaveCount(1);
      await expect(cards.filter({ hasText: 'Comes first' })).toHaveCount(1);
      await expect(cards.filter({ hasText: 'Deploy the thing' })).toHaveCount(1);
      await expect(cards.filter({ hasText: 'Two hops out' })).toHaveCount(0);
      await expect(cards.filter({ hasText: 'Nothing to do with it' })).toHaveCount(0);

      // Why each card is on screen: the hub by its own number badge, the
      // neighbours by the link badge naming the hub.
      const hub = cards.filter({ hasText: 'The hub card' });
      const neighbour = cards.filter({ hasText: 'Comes first' });
      await expect(hub.locator('.card-number')).toHaveClass(/card-number-hit/);
      await expect(neighbour.locator('.card-number')).not.toHaveClass(/card-number-hit/);
      await expect(neighbour.locator('.link-badge-number.link-badge-hit')).toHaveText(
        `#${target.number}`
      );
      // The hub's own badges name the neighbours, which are not what was typed.
      await expect(hub.locator('.link-badge-number.link-badge-hit')).toHaveCount(0);

      // A bare number does not reach along links: the hub alone.
      await search.fill(`${target.number}`);
      await expect(cards).toHaveCount(1);
      await expect(cards).toContainText('The hub card');

      // Other terms still apply to the linked card itself, not to the hub.
      await search.fill(`#${target.number} deploy`);
      await expect(cards).toHaveCount(1);
      await expect(cards).toContainText('Deploy the thing');

      // Liveness: the board still repaints, and the filter lets everything back.
      await search.fill('');
      await expect(cards).toHaveCount(5);
      expect(panics).toEqual([]);
    });

    test('linking and unlinking during an active search moves cards in and out', async ({
      page,
      request,
    }) => {
      const board = await apiCreateBoard(request, `search-link-live-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      const target = await apiCreateCard(request, col.id, 'The hub card');
      const other = await apiCreateCard(request, col.id, 'Joins later');

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      const cards = page.locator('.card-item');
      await expect(cards).toHaveCount(2);

      await page.locator('.navbar-search-input').fill(`#${target.number}`);
      await expect(cards).toHaveCount(1);

      // The link arrives over SSE, with no reload and nothing else touched.
      const link = await apiCreateLink(request, target.id, 'successor', other.id);
      await expect(cards).toHaveCount(2);
      await expect(cards.filter({ hasText: 'Joins later' })).toHaveCount(1);
      await expect(
        cards.filter({ hasText: 'Joins later' }).locator('.link-badge-number.link-badge-hit')
      ).toHaveText(`#${target.number}`);

      // …and removing it takes the card back out, again without a reload.
      await request.delete(`/api/links/${link.id}`);
      await expect(cards).toHaveCount(1);
      await expect(cards).toContainText('The hub card');
      expect(panics).toEqual([]);
    });

    test('an expanded card that matches only by link is not collapsed by the query', async ({
      page,
      request,
    }) => {
      // The #375 rule releases the expanded-card pin when the query moves past
      // the card. A card linked to the number being typed has not been moved
      // past — the filter keeps it — so the pin must hold and the card must
      // stay open.
      const board = await apiCreateBoard(request, `search-link-pin-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      const target = await apiCreateCard(request, col.id, 'The hub card');
      const linked = await apiCreateCard(request, col.id, 'Comes after');
      await apiCreateLink(request, target.id, 'successor', linked.id);

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await expect(page.locator('.card-item')).toHaveCount(2);

      await page.locator('.card-item').filter({ hasText: 'Comes after' }).click();
      const expanded = page.locator('.card-item.card-expanded');
      await expect(expanded).toHaveCount(1);

      await page.locator('.navbar-search-input').fill(`#${target.number}`);
      await expect(page.locator('.card-item')).toHaveCount(2);
      await expect(expanded).toHaveCount(1);
      await expect(expanded).toContainText('Comes after');

      // A number it is *not* linked to still releases it, so the card is held
      // by the link rule and not by the pin refusing to let go at all.
      await page.locator('.navbar-search-input').fill(`#${target.number + 99999}`);
      await expect(page.locator('.card-item')).toHaveCount(0);
      await expect(expanded).toHaveCount(0);

      // Liveness: the board repaints and takes a fresh expansion.
      await page.locator('.navbar-search-input').fill('');
      await expect(page.locator('.card-item')).toHaveCount(2);
      await page.locator('.card-item').filter({ hasText: 'The hub card' }).click();
      await expect(expanded).toContainText('The hub card');
      expect(panics).toEqual([]);
    });
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

  // Card #304: the text-search route into the same disposal race that
  // `card-tags.spec.ts` covers for tags. Editing an expanded card's body past
  // the query used to unmount it mid-edit and wedge the tab; the expanded card
  // is now exempt from the filter until it is collapsed.
  test('editing an expanded card past a text search keeps it open and saves', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `search-expanded-edit-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, '# Deploy checklist');
    await apiCreateCard(request, col.id, '# Deploy notes');

    const panics: string[] = [];
    page.on('console', msg => {
      if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
    });
    page.on('pageerror', err => panics.push(String(err)));

    await gotoBoardView(page, board.name);
    const search = page.locator('.navbar-search-input');
    await search.fill('checklist');
    await expect(page.locator('.card-item')).toHaveCount(1);

    // Expand, then type the body out of the query's reach.
    await page.locator('.card-item').click();
    const expanded = page.locator('.card-item.card-expanded');
    await expanded.locator('.card-markdown, .card-body-placeholder').first().click();
    const textarea = expanded.locator('textarea');
    await textarea.fill('# Something else entirely');
    await textarea.blur();

    // Saved, and still on screen despite no longer matching `checklist`.
    await expect
      .poll(async () => (await apiGetCard(request, card.id)).body)
      .toBe('# Something else entirely');
    await expect(page.locator('.card-item')).toHaveCount(1);
    expect(panics).toEqual([]);

    // Collapsing releases the pin and the card leaves the filtered view.
    await page.locator('.card-toolbar-btn[title="Collapse"]').click();
    await expect(page.locator('.card-item')).toHaveCount(0);

    // The board still reacts — a wedged executor would repaint nothing.
    await search.fill('');
    await expect(page.locator('.card-item')).toHaveCount(2);
    expect(panics).toEqual([]);
  });
});
