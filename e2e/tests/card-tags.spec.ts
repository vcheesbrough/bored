import { test, expect } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiGetCard,
  apiUpdateCard,
  gotoBoardView,
} from './helpers';

// Card tags — iteration 41 / card #292.
//
// Covers the four things the feature promises end to end: tags can be added and
// removed from a card, the `#` search token filters by them, the suggestion
// popup completes them, and every change is its own restorable history row.

test.describe('card tags', () => {
  test('adds a tag from the expanded card and shows it on the collapsed card', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `tags-add-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, '# Taggable card');

    await gotoBoardView(page, board.name);
    await page.locator('.card-item').click();

    const input = page.locator('.tag-editor .tag-editor-input');
    await expect(input).toBeVisible();
    await input.fill('bug');
    await input.press('Enter');

    // The chip appears in the editor and the card round-trips through the API.
    await expect(page.locator('.tag-editor .tag-chip-label')).toHaveText('#bug');
    await expect
      .poll(async () => (await apiGetCard(request, card.id)).tags)
      .toEqual(['bug']);

    // Collapsing shows the read-only chip alongside the number badge.
    await page.locator('.card-toolbar-btn[title="Collapse"]').click();
    await expect(page.locator('.card-item .tag-chip-row .tag-chip')).toHaveText('#bug');
  });

  test('removes a tag with the chip × button', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `tags-remove-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, '# Tagged card', ['bug', 'urgent']);

    await gotoBoardView(page, board.name);
    await page.locator('.card-item').click();

    await expect(page.locator('.tag-editor .tag-chip')).toHaveCount(2);
    await page.locator('.tag-chip-remove[aria-label="Remove tag bug"]').click();

    await expect(page.locator('.tag-editor .tag-chip')).toHaveCount(1);
    await expect
      .poll(async () => (await apiGetCard(request, card.id)).tags)
      .toEqual(['urgent']);
  });

  test('normalizes a typed tag the way the server does', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `tags-normalize-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, '# Normalize me', ['bug']);

    await gotoBoardView(page, board.name);
    await page.locator('.card-item').click();

    // A leading `#` is stripped, and a case-variant duplicate is dropped.
    const input = page.locator('.tag-editor .tag-editor-input');
    await input.fill('#BUG');
    await input.press('Enter');

    await expect(page.locator('.tag-editor .tag-chip')).toHaveCount(1);
    await expect(page.locator('.tag-editor .tag-chip-label')).toHaveText('#bug');
    expect((await apiGetCard(request, card.id)).tags).toEqual(['bug']);
  });

  test('the tag suggestion popup follows focus on the + tag input', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `tags-popup-focus-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, '# Already tagged', ['backend', 'urgent']);
    await apiCreateCard(request, col.id, '# Needs a tag');

    await gotoBoardView(page, board.name);
    await page.locator('.card-item').filter({ hasText: 'Needs a tag' }).click();

    const popup = page.locator('.tag-suggestions');
    const options = page.locator('.tag-suggestions .tag-suggestion');
    const input = page.locator('.tag-editor .tag-editor-input');

    // Expanding the card alone must not open the popup.
    await expect(input).toBeVisible();
    await expect(popup).toHaveCount(0);

    // Focus opens it, offering the board's tags to browse with nothing typed.
    await input.click();
    await expect(input).toBeFocused();
    await expect(popup).toBeVisible();
    await expect(options).toHaveText(['#backend', '#urgent']);

    // Typing narrows the same list.
    await input.fill('back');
    await expect(options).toHaveText(['#backend']);

    // Clearing restores the full list rather than closing the popup.
    await input.fill('');
    await expect(options).toHaveText(['#backend', '#urgent']);

    // Losing focus closes it.
    await input.blur();
    await expect(popup).toHaveCount(0);
  });

  test('suggests tags already in use on the board', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `tags-suggest-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, '# Already tagged', ['backend']);
    const target = await apiCreateCard(request, col.id, '# Needs a tag');

    await gotoBoardView(page, board.name);
    await page
      .locator('.card-item')
      .filter({ hasText: 'Needs a tag' })
      .click();

    const input = page.locator('.tag-editor .tag-editor-input');
    await input.fill('back');

    const suggestion = page.locator('.tag-suggestions .tag-suggestion');
    await expect(suggestion).toHaveText('#backend');

    await suggestion.click();
    await expect
      .poll(async () => (await apiGetCard(request, target.id)).tags)
      .toEqual(['backend']);
  });

  test('#tag search filters the board and highlights the matching chip', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `tags-search-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, '# Tagged bug card', ['bug']);
    await apiCreateCard(request, col.id, '# Untagged card');
    await apiCreateCard(request, col.id, '# Chore card', ['chore']);

    await gotoBoardView(page, board.name);
    await expect(page.locator('.card-item')).toHaveCount(3);

    const search = page.locator('.navbar-search-input');
    await search.fill('#bug ');

    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-item')).toContainText('Tagged bug card');
    // The matching chip inverts, the way the number badge does for `#42`.
    const chip = page.locator('.card-item .tag-chip');
    await expect(chip).toHaveClass(/tag-chip-hit/);
    await expect(chip).toHaveCSS('background-color', 'rgb(251, 191, 36)');

    // Clearing restores every card.
    await search.fill('');
    await expect(page.locator('.card-item')).toHaveCount(3);
  });

  test('a #tag term ANDs with free text', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `tags-search-and-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, '# Deploy preview', ['bug']);
    await apiCreateCard(request, col.id, '# Deploy staging', ['chore']);
    await apiCreateCard(request, col.id, '# Release notes', ['bug']);

    await gotoBoardView(page, board.name);
    await page.locator('.navbar-search-input').fill('#bug deploy');

    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-item')).toContainText('Deploy preview');
  });

  test('typing # in search opens a popup of tags and card numbers', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `tags-search-popup-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, '# Popup card', ['frontend']);

    await gotoBoardView(page, board.name);
    const search = page.locator('.navbar-search-input');

    // A bare `#` offers both kinds of completion and filters nothing yet.
    await search.fill('#');
    const options = page.locator('.search-suggestions .search-suggestion');
    await expect(options.filter({ hasText: '#frontend' })).toHaveCount(1);
    await expect(options.filter({ hasText: `#${card.number}` })).toHaveCount(1);
    await expect(page.locator('.card-item')).toHaveCount(1);

    // Picking the tag rewrites the open token and applies the filter.
    await options.filter({ hasText: '#frontend' }).click();
    await expect(search).toHaveValue('#frontend ');
    await expect(page.locator('.card-item')).toHaveCount(1);
    // The popup closes once the token is complete.
    await expect(page.locator('.search-suggestions')).toHaveCount(0);
  });

  test('the # popup is keyboard-driven and Escape dismisses it before clearing', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `tags-search-keys-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, '# Keyboard card', ['backend']);

    await gotoBoardView(page, board.name);
    const search = page.locator('.navbar-search-input');
    await search.fill('#back');

    const options = page.locator('.search-suggestions .search-suggestion');
    await expect(options).toHaveCount(1);

    // Escape closes the popup but leaves the query intact…
    await search.press('Escape');
    await expect(page.locator('.search-suggestions')).toHaveCount(0);
    await expect(search).toHaveValue('#back');

    // …and a further Escape clears the query as it always did.
    await search.press('Escape');
    await expect(search).toHaveValue('');

    // Arrow keys highlight a row; Enter accepts it.
    await search.fill('#back');
    await expect(options).toHaveCount(1);
    await search.press('ArrowDown');
    await expect(options.first()).toHaveClass(/search-suggestion-active/);
    await search.press('Enter');
    await expect(search).toHaveValue('#backend ');
  });

  test('tag changes are their own history rows and restore body and tags together', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `tags-history-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, '# Versioned card', ['first']);

    // Two distinct changes: tags only, then tags again.
    await apiUpdateCard(request, card.id, { tags: ['first', 'second'] });
    await apiUpdateCard(request, card.id, { tags: ['second'] });

    await gotoBoardView(page, board.name);
    await page.locator('.card-item').click();
    await page.locator('.card-float-panel [title="Card history"]').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const rows = page.locator(`.history-row[data-entity-id="${card.id}"]`);
    // create + two tag updates — the tag edits never merged into one row, and
    // each got its own verb. Newest first, so the removal leads.
    //
    // Asserting the headlines as an ordered list rather than filtering by text:
    // Playwright's `hasText` is a case-insensitive substring match, so
    // "Untagged card …" also matches a filter for "Tagged card …".
    await expect(rows.locator('.history-headline')).toHaveText([
      'Untagged card «Versioned card»',
      'Tagged card «Versioned card»',
      'Created card «Versioned card»',
    ]);

    await expect(rows.nth(0).locator('.history-sub')).toHaveText(
      `Card #${card.number} · −first`
    );
    await expect(rows.nth(1).locator('.history-sub')).toHaveText(
      `Card #${card.number} · +second`
    );

    // Restoring the create row puts the original tag list back.
    const createRow = rows.filter({ has: page.locator('.history-badge-create') });
    await createRow.getByRole('button', { name: 'Preview' }).click();
    await expect(createRow.locator('.history-version-tags .tag-chip')).toHaveText('#first');

    await createRow.getByRole('button', { name: 'Restore version' }).click();
    await expect
      .poll(async () => (await apiGetCard(request, card.id)).tags)
      .toEqual(['first']);
  });

  test('tags added elsewhere arrive over SSE', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `tags-sse-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, '# Remote tag card');

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
    await expect(page.locator('.card-item .tag-chip')).toHaveCount(0);

    await apiUpdateCard(request, card.id, { tags: ['remote'] });
    await expect(page.locator('.card-item .tag-chip')).toHaveText('#remote', {
      timeout: 5000,
    });

    await context.close();
  });

  test('the modal edits the same tags as the inline card', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `tags-modal-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const card = await apiCreateCard(request, col.id, '# Modal card', ['bug']);

    await gotoBoardView(page, board.name);
    await page.locator('.card-item').click();
    await page.locator('.card-toolbar-btn[title="Maximise"]').click();
    await expect(page.locator('.modal')).toBeVisible();

    const input = page.locator('.modal .tag-editor .tag-editor-input');
    await input.fill('urgent');
    await input.press('Enter');

    await expect(page.locator('.modal .tag-editor .tag-chip')).toHaveCount(2);
    await expect
      .poll(async () => (await apiGetCard(request, card.id)).tags)
      .toEqual(['bug', 'urgent']);
  });

  // ── Editing an expanded card out of the active filter — card #304 ────────
  //
  // Removing the tag you are filtering on used to unmount the expanded card
  // mid-edit, which disposed its signals while its own render closures were
  // still queued to read them. The resulting panic fired inside the
  // `wasm-bindgen-futures` task queue and wedged the executor: the in-flight PUT
  // never ran and no effect ever ran again, so the tab was dead until a reload.
  //
  // Two changes hold it shut, and these tests pin both: the expanded card is
  // exempt from the filter while it is open, and the reads that trapped are
  // `try_get`. Every test therefore asserts three things — no WASM panic, the
  // change actually reached the server, and the board still repaints afterwards.
  test.describe('editing an expanded card out of the filter', () => {
    /**
     * Collect WASM panics. A trap surfaces as a `wasm panic:` console error and
     * an `unreachable` page error; either one is a failure.
     */
    function watchForPanics(page: import('@playwright/test').Page) {
      const panics: string[] = [];
      page.on('console', msg => {
        if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
      });
      page.on('pageerror', err => panics.push(String(err)));
      return panics;
    }

    /**
     * The board is only truly alive if it still reacts. Clearing the search has
     * to repaint every card — under a wedged executor nothing repaints at all.
     */
    async function expectBoardStillLive(
      page: import('@playwright/test').Page,
      totalCards: number
    ) {
      await page.locator('.navbar-search-input').fill('');
      await expect(page.locator('.card-item')).toHaveCount(totalCards);
    }

    test('removing the filtered tag saves, keeps the card, and leaves the tab alive', async ({
      page,
      request,
    }) => {
      const board = await apiCreateBoard(request, `tags-filter-remove-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      const card = await apiCreateCard(request, col.id, '# Filtered card', ['bug']);
      await apiCreateCard(request, col.id, '# Other card', ['bug']);

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await page.locator('.navbar-search-input').fill('#bug');
      await expect(page.locator('.card-item')).toHaveCount(2);

      await page.locator('.card-item').filter({ hasText: 'Filtered card' }).click();
      const expanded = page.locator('.card-item.card-expanded');
      await expect(expanded).toBeVisible();
      await expanded.locator('.tag-chip-remove').first().click();

      // The PUT lands — this is the "it doesn't work" half of the report.
      await expect.poll(async () => (await apiGetCard(request, card.id)).tags).toEqual([]);
      // The card you are editing stays put even though it no longer matches.
      await expect(expanded.filter({ hasText: 'Filtered card' })).toHaveCount(1);
      expect(panics).toEqual([]);
      await expectBoardStillLive(page, 2);
    });

    test('collapsing a pinned card drops it from the filtered view without trapping', async ({
      page,
      request,
    }) => {
      // The pin moves the unmount to collapse time, where it coincides with the
      // `card_state` / `expanded_card_id` write — the same shape as the original
      // bug, so it is proven rather than assumed.
      const board = await apiCreateBoard(request, `tags-filter-collapse-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      await apiCreateCard(request, col.id, '# Filtered card', ['bug']);
      await apiCreateCard(request, col.id, '# Other card', ['bug']);

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await page.locator('.navbar-search-input').fill('#bug');
      await page.locator('.card-item').filter({ hasText: 'Filtered card' }).click();
      await page.locator('.card-item.card-expanded .tag-chip-remove').first().click();
      await expect(page.locator('.card-item.card-expanded .tag-chip')).toHaveCount(0);

      await page.locator('.card-toolbar-btn[title="Collapse"]').click();

      // Collapsed, it no longer matches `#bug`, so now it goes.
      await expect(page.locator('.card-item').filter({ hasText: 'Filtered card' })).toHaveCount(0);
      await expect(page.locator('.card-item')).toHaveCount(1);
      expect(panics).toEqual([]);
      await expectBoardStillLive(page, 2);
    });

    test('expanding another card while a pinned card is mid-edit saves it and does not trap', async ({
      page,
      request,
    }) => {
      // The pin does not remove the unmount — it moves it to the moment another
      // card claims the board's expanded lock. That write makes two subscribers
      // dirty at once: the column's filter, which drops and disposes the pinned
      // card, and the pinned card's own effect, which flushes the unsaved body.
      // So the flush and the disposal race, and the debounce future started by
      // the last keystroke is still asleep when the card goes. All three have to
      // tolerate it: the edit must reach the server and nothing may trap.
      const board = await apiCreateBoard(request, `tags-filter-switch-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      const a = await apiCreateCard(request, col.id, '# Card A', ['bug']);
      await apiCreateCard(request, col.id, '# Card B', ['bug']);

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await page.locator('.navbar-search-input').fill('#bug');

      // Expand A and remove the filtered tag, so A is pinned and unmatching.
      await page.locator('.card-item').filter({ hasText: 'Card A' }).click();
      const expanded = page.locator('.card-item.card-expanded');
      await expanded.locator('.tag-chip-remove').first().click();
      await expect(expanded.locator('.tag-chip')).toHaveCount(0);

      // Type into A without waiting for the debounce to fire.
      await expanded.locator('.card-markdown, .card-body-placeholder').first().click();
      await expanded.locator('textarea').fill('# Card A\nedited while pinned');

      // Claim the lock with B, never collapsing A.
      await page.locator('.card-item').filter({ hasText: 'Card B' }).click();

      // A's edit is flushed on the way out rather than dropped.
      await expect
        .poll(async () => (await apiGetCard(request, a.id)).body)
        .toBe('# Card A\nedited while pinned');
      // A has left the filtered view now that it is no longer pinned.
      await expect(page.locator('.card-item').filter({ hasText: 'Card A' })).toHaveCount(0);
      expect(panics).toEqual([]);
      await expectBoardStillLive(page, 2);
    });

    test('a remote edit that unmatches the expanded card does not trap', async ({
      page,
      request,
    }) => {
      // Another client or an MCP agent editing the card you have open reaches
      // the same disposal race through the SSE handler instead of a click.
      const board = await apiCreateBoard(request, `tags-filter-remote-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      const card = await apiCreateCard(request, col.id, '# Filtered card', ['bug']);
      await apiCreateCard(request, col.id, '# Other card', ['bug']);

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await page.locator('.navbar-search-input').fill('#bug');
      await page.locator('.card-item').filter({ hasText: 'Filtered card' }).click();
      await expect(page.locator('.card-item.card-expanded')).toBeVisible();

      await apiUpdateCard(request, card.id, { tags: [] });

      // Pinned, so it survives the remote edit; the tag chip goes, the card stays.
      await expect(page.locator('.card-item.card-expanded .tag-chip')).toHaveCount(0, {
        timeout: 5000,
      });
      await expect(
        page.locator('.card-item.card-expanded').filter({ hasText: 'Filtered card' })
      ).toHaveCount(1);
      expect(panics).toEqual([]);
      await expectBoardStillLive(page, 2);
    });
  });
});
