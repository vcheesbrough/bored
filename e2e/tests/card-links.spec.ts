import { test, expect } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiCreateLink,
  apiDeleteCard,
  apiDeleteColumn,
  apiListLinks,
  apiMoveCard,
  closeChooser,
  gotoBoardView,
  openChooser,
} from './helpers';

// Card predecessor/successor links — iteration 42 / card #76.
//
// A link is one fact visible from both cards: "A comes before B" shows up in
// A's "after" group and in B's "before" group. These tests cover creating and
// removing links from the editor, the loop guard, the error line, the
// collapsed-card badge, SSE delivery, and the modal sharing the inline
// card's links.

/** The expanded card whose body contains `text`. */
function cardWith(page: import('@playwright/test').Page, text: string) {
  return page.locator('.card-item').filter({ hasText: text });
}

test.describe('card links', () => {
  test('links two cards from the editor and shows the link on both', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `links-add-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const a = await apiCreateCard(request, col.id, '# Alpha card');
    const b = await apiCreateCard(request, col.id, '# Beta card');

    await gotoBoardView(page, board.name);
    await cardWith(page, 'Alpha card').click();

    // Pick Beta as a card that comes *after* Alpha.
    const after = page.locator('.link-group[data-side="after"]');
    const input = after.locator('.link-picker-input');
    await input.fill('beta');
    const option = page.locator('.link-suggestions .link-suggestion');
    await expect(option).toHaveText(`#${b.number} Beta card`);
    await option.click();

    // The chip appears on Alpha's "after" side and the link round-trips. The
    // column name is a sibling span, so the button's own text is unchanged.
    await expect(after.locator('.link-chip-card')).toHaveText(`#${b.number} Beta card`);
    await expect(after.locator('.link-chip-column')).toHaveText('Todo');
    await expect.poll(async () => apiListLinks(request, board.name)).toEqual([
      expect.objectContaining({ predecessor_id: a.id, successor_id: b.id, reason: null }),
    ]);

    // Collapsing shows a pill per link: Alpha has Beta after it, Beta has
    // Alpha before it.
    await page.locator('.card-toolbar-btn[title="Collapse"]').click();
    await expect(cardWith(page, 'Alpha card').locator('.link-badge-after')).toHaveText(
      `↓#${b.number}`
    );
    await expect(cardWith(page, 'Beta card').locator('.link-badge-before')).toHaveText(
      `↑#${a.number}`
    );

    // The same link is visible — and editable — from Beta's side.
    await cardWith(page, 'Beta card').click();
    await expect(
      page.locator('.link-group[data-side="before"] .link-chip-card')
    ).toHaveText(`#${a.number} Alpha card`);
    await expect(
      page.locator('.link-group[data-side="before"] .link-chip-column')
    ).toHaveText('Todo');
  });

  test('a chip names the column its card is in, and follows it when it moves', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `links-column-${Date.now()}`);
    const todo = await apiCreateColumn(request, board.name, 'Todo', 0);
    const doing = await apiCreateColumn(request, board.name, 'Doing', 1);
    const done = await apiCreateColumn(request, board.name, 'Done', 2);
    const a = await apiCreateCard(request, todo.id, '# Waiting card');
    const b = await apiCreateCard(request, doing.id, '# Blocker card');
    // Blocker comes before Waiting, so it shows on Waiting's "before" side.
    await apiCreateLink(request, b.id, 'successor', a.id);

    await gotoBoardView(page, board.name);
    await cardWith(page, 'Waiting card').click();

    // The prefix is the linked card's column, not the open card's.
    const chip = page.locator('.link-group[data-side="before"] .link-chip');
    await expect(chip.locator('.link-chip-card')).toHaveText(`#${b.number} Blocker card`);
    await expect(chip.locator('.link-chip-column')).toHaveText('Doing');

    // Moving the linked card updates the prefix in place — this is the whole
    // point of the prefix, so it must not need a reload to be right.
    await apiMoveCard(request, b.id, done.id);
    await expect(chip.locator('.link-chip-column')).toHaveText('Done', { timeout: 5000 });
  });

  test('a card that would close a loop is not offered, and the server refuses it', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `links-cycle-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const a = await apiCreateCard(request, col.id, '# First');
    const b = await apiCreateCard(request, col.id, '# Second');
    const c = await apiCreateCard(request, col.id, '# Third');
    await apiCreateLink(request, a.id, 'successor', b.id);

    await gotoBoardView(page, board.name);
    await cardWith(page, 'Second').click();

    // Focusing the "after" picker on Second lists Third but not First: First
    // → Second already exists, so Second → First would be a loop. Second
    // itself is never offered either.
    await page.locator('.link-group[data-side="after"] .link-picker-input').click();
    const options = page.locator('.link-suggestions .link-suggestion');
    await expect(options).toHaveText([`#${c.number} Third`]);

    // Asked directly, the API says why.
    const res = await request.post(`/api/cards/${b.id}/links`, {
      data: { direction: 'successor', other_card_id: a.id },
    });
    expect(res.status()).toBe(422);
    expect(await res.text()).toContain('loop');
  });

  test('a refused link shows the server’s reason in the editor', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `links-error-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    await apiCreateCard(request, col.id, '# Origin');
    await apiCreateCard(request, col.id, '# Target');

    await gotoBoardView(page, board.name);

    // The picker only offers what the server should accept, so the refusal
    // is simulated the way it would really happen: another client changed the
    // graph between this board loading and the request landing.
    await page.route('**/api/cards/*/links', route =>
      route.fulfill({
        status: 422,
        contentType: 'text/plain',
        body: 'linking these cards would create a loop',
      })
    );

    await cardWith(page, 'Origin').click();
    await page.locator('.link-group[data-side="after"] .link-picker-input').fill('target');
    await page.locator('.link-suggestions .link-suggestion').click();

    await expect(page.locator('.link-editor-error')).toHaveText(
      'linking these cards would create a loop'
    );
    await expect(page.locator('.link-chip')).toHaveCount(0);
  });

  test('removing a link clears it from both cards', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `links-remove-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const a = await apiCreateCard(request, col.id, '# Upstream');
    const b = await apiCreateCard(request, col.id, '# Downstream');
    await apiCreateLink(request, a.id, 'successor', b.id, 'because');

    await gotoBoardView(page, board.name);
    await expect(cardWith(page, 'Downstream').locator('.link-badge-before')).toHaveText(
      `↑#${a.number}`
    );

    await cardWith(page, 'Upstream').click();
    const chip = page.locator('.link-group[data-side="after"] .link-chip');
    await expect(chip.locator('.link-chip-reason')).toHaveText('because');
    await chip.locator(`.link-chip-remove[aria-label="Remove link to #${b.number}"]`).click();

    await expect(page.locator('.link-chip')).toHaveCount(0);
    await expect.poll(async () => apiListLinks(request, board.name)).toEqual([]);

    // The other end lost its badge too.
    await page.locator('.card-toolbar-btn[title="Collapse"]').click();
    await expect(cardWith(page, 'Downstream').locator('.link-badge')).toHaveCount(0);
  });

  test('a link reason can be set from the chip', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `links-reason-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const a = await apiCreateCard(request, col.id, '# Needs');
    const b = await apiCreateCard(request, col.id, '# Provides');
    await apiCreateLink(request, b.id, 'successor', a.id);

    await gotoBoardView(page, board.name);
    await cardWith(page, 'Needs').click();

    const chip = page.locator('.link-group[data-side="before"] .link-chip');
    await chip.locator('.link-chip-edit').click();
    const reason = chip.locator('.link-reason-input');
    await expect(reason).toBeFocused();
    await reason.fill('the API has to exist first');
    await reason.press('Enter');

    await expect(chip.locator('.link-chip-reason')).toHaveText('the API has to exist first');
    await expect
      .poll(async () => (await apiListLinks(request, board.name))[0]?.reason)
      .toBe('the API has to exist first');
    // Enter and the blur it causes must not race a second save; a duplicate
    // request would surface here as an error line.
    await expect(page.locator('.link-editor-error')).toHaveCount(0);
  });

  test('the collapsed card shows one pill per linked card on each side', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `links-badge-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const hub = await apiCreateCard(request, col.id, '# Hub');
    const in1 = await apiCreateCard(request, col.id, '# In one');
    const in2 = await apiCreateCard(request, col.id, '# In two');
    const out = await apiCreateCard(request, col.id, '# Out');
    await apiCreateLink(request, in1.id, 'successor', hub.id);
    await apiCreateLink(request, in2.id, 'successor', hub.id);
    await apiCreateLink(request, hub.id, 'successor', out.id);

    await gotoBoardView(page, board.name);
    const badges = cardWith(page, 'Hub').locator('.card-link-badges');
    // Oldest link first — one pill per card, not a count.
    await expect(badges.locator('.link-badge-before')).toHaveText([
      `↑#${in1.number}`,
      `↑#${in2.number}`,
    ]);
    await expect(badges.locator('.link-badge-after')).toHaveText([`↓#${out.number}`]);
    // A card with links on one side only shows that side.
    await expect(cardWith(page, 'Out').locator('.link-badge')).toHaveText([`↑#${hub.number}`]);
    await expect(cardWith(page, 'In one').locator('.link-badge')).toHaveText([
      `↓#${hub.number}`,
    ]);
  });

  test('links added elsewhere arrive over SSE', async ({ browser, request }) => {
    const board = await apiCreateBoard(request, `links-sse-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const a = await apiCreateCard(request, col.id, '# Remote A');
    const b = await apiCreateCard(request, col.id, '# Remote B');

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
    await expect(page.locator('.link-badge')).toHaveCount(0);

    const link = await apiCreateLink(request, a.id, 'successor', b.id);
    await expect(cardWith(page, 'Remote A').locator('.link-badge-after')).toHaveText(
      `↓#${b.number}`,
      { timeout: 5000 }
    );
    await expect(cardWith(page, 'Remote B').locator('.link-badge-before')).toHaveText(
      `↑#${a.number}`
    );

    // Removal arrives the same way.
    await request.delete(`/api/links/${link.id}`);
    await expect(page.locator('.link-badge')).toHaveCount(0, { timeout: 5000 });

    await context.close();
  });

  test('the modal edits the same links as the inline card', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `links-modal-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const a = await apiCreateCard(request, col.id, '# Modal origin');
    const b = await apiCreateCard(request, col.id, '# Modal target');

    await gotoBoardView(page, board.name);
    await cardWith(page, 'Modal origin').click();
    await page.locator('.card-toolbar-btn[title="Maximise"]').click();
    await expect(page.locator('.modal')).toBeVisible();

    const picker = page.locator('.modal .link-group[data-side="after"] .link-picker-input');
    await picker.fill(String(b.number));
    await page.locator('.modal .link-suggestions .link-suggestion').click();
    await expect(page.locator('.modal .link-group[data-side="after"] .link-chip-card')).toHaveText(
      `#${b.number} Modal target`
    );
    // The maximised surface carries the column prefix too.
    await expect(
      page.locator('.modal .link-group[data-side="after"] .link-chip-column')
    ).toHaveText('Todo');
    await expect.poll(async () => apiListLinks(request, board.name)).toEqual([
      expect.objectContaining({ predecessor_id: a.id, successor_id: b.id }),
    ]);

    // Back on the board, the inline card shows the link the modal made.
    await page.locator('.modal .card-toolbar-btn[title="Restore to board"]').click();
    await expect(page.locator('.modal')).toHaveCount(0);
    await cardWith(page, 'Modal origin').click();
    await expect(
      page.locator('.card-item .link-group[data-side="after"] .link-chip-card')
    ).toHaveText(`#${b.number} Modal target`);
  });

  test('link changes are history rows on both cards', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `links-history-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const a = await apiCreateCard(request, col.id, '# Hist A');
    const b = await apiCreateCard(request, col.id, '# Hist B');
    const link = await apiCreateLink(request, a.id, 'successor', b.id, 'why');
    await request.delete(`/api/links/${link.id}`);

    await gotoBoardView(page, board.name);
    // Open history from the card that was never itself edited.
    await cardWith(page, 'Hist B').click();
    await page.locator('.card-float-panel [title="Card history"]').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const rows = page.locator(`.history-row[data-entity-id="${link.id}"]`);
    await expect(rows.locator('.history-headline')).toHaveText([
      `Unlinked #${a.number} → #${b.number}`,
      `Linked #${a.number} → #${b.number}`,
    ]);
    await expect(rows.nth(1).locator('.history-sub')).toHaveText('«why»');
    // A removed link is not restorable, so no control is offered.
    await expect(rows.getByRole('button', { name: 'Restore' })).toHaveCount(0);
  });

  // ── Deleting a linked card that receives no broadcast — card #313 ───────
  //
  // The board prunes its link index locally now instead of waiting for the
  // server's `CardLinkDeleted` events. These tests hold the browser to that by
  // subscribing the page to a board that does not exist: the stream is open and
  // healthy, the keepalives arrive, and not one event for *this* board ever
  // does — so every assertion below is about what the tab did for itself. That
  // is the real-world case this stands in for: a receiver lagging out of the
  // backend's 128-slot broadcast channel loses events with the connection still
  // up.
  //
  // It used to abort `/api/events` outright. Since iteration 58 a dead stream
  // means the tab is disconnected, and a disconnected tab refuses every
  // mutation (frontend/src/connection.rs) — so an aborted stream can no longer
  // reach the delete these tests are about, and would test the refusal instead.
  test.describe('deleting a linked card with no broadcast', () => {
    /**
     * Point the event stream at a board id nothing will ever publish to. The
     * backend filters by that id and validates nothing, so the response is a
     * perfectly ordinary, perfectly silent SSE stream.
     */
    async function silenceSse(page: import('@playwright/test').Page) {
      await page.route('**/api/events*', route => {
        const url = new URL(route.request().url());
        url.searchParams.set('board_id', 'no-such-board');
        return route.continue({ url: url.toString() });
      });
    }

    /**
     * Watch for reactive-disposal panics. Pruning the link index on delete
     * notifies the badges of the card being unmounted, so this path is one trap
     * away from a wedged tab — and a wedged tab fails silently, by making every
     * later interaction inert rather than by raising anything at the assertion.
     */
    function watchForPanics(page: import('@playwright/test').Page) {
      const panics: string[] = [];
      page.on('console', msg => {
        if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
      });
      page.on('pageerror', err => panics.push(String(err)));
      return panics;
    }

    test('an inline delete clears the partner card of the deleted card', async ({
      page,
      request,
    }) => {
      const board = await apiCreateBoard(request, `links-del-inline-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      const doomed = await apiCreateCard(request, col.id, '# Doomed card');
      const partner = await apiCreateCard(request, col.id, '# Partner card');
      // One link on each side of the partner, so the fix has to prune by *both*
      // ends rather than only the side it happens to look at first.
      await apiCreateLink(request, doomed.id, 'successor', partner.id);
      const other = await apiCreateCard(request, col.id, '# Third card');
      await apiCreateLink(request, partner.id, 'successor', other.id);

      const panics = watchForPanics(page);
      await silenceSse(page);
      await gotoBoardView(page, board.name);
      await expect(cardWith(page, 'Partner card').locator('.link-badge-before')).toHaveText(
        `↑#${doomed.number}`
      );

      await cardWith(page, 'Doomed card').click();
      await page.locator('.card-toolbar-close').first().click();
      await page.locator('.btn-danger').click();

      // The card goes, and so does its link — with no reload and no event.
      await expect(cardWith(page, 'Doomed card')).toHaveCount(0);
      await expect(cardWith(page, 'Partner card').locator('.link-badge-before')).toHaveCount(0);
      // The partner's *other* link is untouched: pruning is by card, not a
      // blanket clear.
      await expect(cardWith(page, 'Partner card').locator('.link-badge-after')).toHaveText(
        `↓#${other.number}`
      );
      // No chip degraded to a bare `#N` — the symptom the card reported.
      // Assert the card actually expanded first: a bare `toHaveCount(0)` on the
      // chips passes just as well when the editor is absent, which would hide a
      // wedged tab (a disposal panic leaves every later click inert).
      await cardWith(page, 'Partner card').click();
      const expandedPartner = page.locator('.card-item.card-expanded');
      await expect(expandedPartner).toHaveCount(1);
      await expect(expandedPartner.locator('.link-group')).toHaveCount(2);
      await expect(
        expandedPartner.locator('.link-group[data-side="before"] .link-chip')
      ).toHaveCount(0);
      await expect(
        expandedPartner.locator('.link-group[data-side="after"] .link-chip-card')
      ).toHaveText(`#${other.number} Third card`);
      // The server agrees: the cascade really did remove only that link.
      await expect.poll(async () => apiListLinks(request, board.name)).toEqual([
        expect.objectContaining({ predecessor_id: partner.id, successor_id: other.id }),
      ]);
      expect(panics).toEqual([]);
    });

    test('a delete from the maximised modal removes the card and its links', async ({
      page,
      request,
    }) => {
      const board = await apiCreateBoard(request, `links-del-modal-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      const doomed = await apiCreateCard(request, col.id, '# Doomed card');
      const partner = await apiCreateCard(request, col.id, '# Partner card');
      await apiCreateLink(request, doomed.id, 'successor', partner.id);

      const panics = watchForPanics(page);
      await silenceSse(page);
      await gotoBoardView(page, board.name);
      await cardWith(page, 'Doomed card').click();
      await page.locator('.card-toolbar-btn[title="Maximise"]').click();
      await expect(page.locator('.modal')).toBeVisible();

      await page.locator('.modal .card-toolbar-close').first().click();
      await page.locator('.btn-danger').click();

      // `on_modal_delete` used to be a no-op, so with no event arriving both
      // the card and its link survived here.
      await expect(page.locator('.modal')).toHaveCount(0);
      await expect(cardWith(page, 'Doomed card')).toHaveCount(0);
      await expect(cardWith(page, 'Partner card').locator('.link-badge')).toHaveCount(0);
      await expect.poll(async () => apiListLinks(request, board.name)).toEqual([]);
      expect(panics).toEqual([]);
    });
  });

  test('a lagged tab heals a deleted card\'s links from the card event alone', async ({
    browser,
    request,
  }) => {
    // Covers the SSE `CardDeleted` arm, which nothing else reaches. The
    // SSE-down tests abort the stream entirely, and *any* healthy stream —
    // local delete or remote — has already pruned the index through the
    // ordinary `CardLinkDeleted` handler before this arm runs, because the
    // backend broadcasts the link removals first. Verified by commenting the
    // arm out: every other test in this file still passed.
    //
    // The arm exists for one situation only: a receiver that lagged out of the
    // backend's 128-slot broadcast channel, losing the `card_link_deleted`
    // events while still getting `card_deleted`. That is simulated faithfully
    // here by dropping exactly those messages on the way into the app — the
    // page sees precisely what a lagged receiver sees.
    //
    // The partner is left **expanded**, so the prune notifies a live
    // `LinkChip` and `LinkBadges` from the SSE handler rather than from a
    // click. Those are the components this iteration found disposal traps in,
    // so a path that notifies them is exactly what needs asserting.
    const board = await apiCreateBoard(request, `links-lagged-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const doomed = await apiCreateCard(request, col.id, '# Doomed card');
    const partner = await apiCreateCard(request, col.id, '# Partner card');
    const other = await apiCreateCard(request, col.id, '# Third card');
    await apiCreateLink(request, doomed.id, 'successor', partner.id, 'because');
    await apiCreateLink(request, partner.id, 'successor', other.id);

    const context = await browser.newContext();
    const page = await context.newPage();
    const panics: string[] = [];
    page.on('console', msg => {
      if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
    });
    page.on('pageerror', err => panics.push(String(err)));

    // Swallow every `card_link_deleted` before the app can see it. The board
    // installs a single `onmessage` handler, so wrapping that setter is enough.
    await page.addInitScript(() => {
      const Native = window.EventSource;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (window as any).EventSource = function (...args: unknown[]) {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const es = new (Native as any)(...args);
        Object.defineProperty(es, 'onmessage', {
          set(handler: (ev: MessageEvent) => void) {
            Native.prototype.addEventListener.call(es, 'message', (ev: Event) => {
              const msg = ev as MessageEvent;
              try {
                if (JSON.parse(msg.data)?.type === 'card_link_deleted') return;
              } catch {
                /* not JSON — pass it through untouched */
              }
              handler(msg);
            });
          },
          configurable: true,
        });
        return es;
      };
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (window as any).EventSource.prototype = Native.prototype;
    });

    const eventsReady = page.waitForResponse(
      response =>
        response.request().method() === 'GET' &&
        response.url().includes(`/api/events?board_id=${board.id}`) &&
        response.ok()
    );
    await gotoBoardView(page, board.name);
    await eventsReady;

    // Open the partner so its chips are mounted when the delete arrives.
    await cardWith(page, 'Partner card').click();
    const expanded = page.locator('.card-item.card-expanded');
    await expect(
      expanded.locator('.link-group[data-side="before"] .link-chip-card')
    ).toHaveText(`#${doomed.number} Doomed card`);

    await apiDeleteCard(request, doomed.id);

    // The link goes even though this tab never saw its removal event — the
    // card event alone is enough, which is the whole point of the arm.
    await expect(
      expanded.locator('.link-group[data-side="before"] .link-chip')
    ).toHaveCount(0, { timeout: 5000 });
    await expect(cardWith(page, 'Doomed card')).toHaveCount(0);
    // The partner's unrelated link is untouched, and still rendered.
    await expect(
      expanded.locator('.link-group[data-side="after"] .link-chip-card')
    ).toHaveText(`#${other.number} Third card`);
    await expect.poll(async () => apiListLinks(request, board.name)).toEqual([
      expect.objectContaining({ predecessor_id: partner.id, successor_id: other.id }),
    ]);
    expect(panics).toEqual([]);

    // The tab still reacts — a wedged executor would repaint nothing. A query
    // matching neither card empties the board, the expanded partner included:
    // card #304's pin holds a card against its own edits, not against the
    // search moving (card #375), so the query change lets go of it. Clearing
    // the search then has to bring both survivors back, the partner collapsed
    // now that the search released it — two repaints a dead tab cannot make.
    await page.locator('.navbar-search-input').fill('matches no card at all');
    await expect(page.locator('.card-item')).toHaveCount(0);
    await page.locator('.navbar-search-input').fill('');
    await expect(page.locator('.card-item')).toHaveCount(2);
    await expect(page.locator('.card-item.card-expanded')).toHaveCount(0);
    expect(panics).toEqual([]);

    await context.close();
  });

  // ── Deleting a column that holds linked cards — card #371 ───────────────
  //
  // #313 one level up. A column delete cascades on the server to its cards and
  // every link touching them; the server broadcasts one `card_link_deleted` per
  // link and then a single `column_deleted` — no `card_deleted` for the cards.
  // The browser used to apply only the column removal itself, so a tab that
  // missed the link events kept every link of every card in the column, and
  // partner cards in *surviving* columns went on showing badges and bare `#N`
  // chips until a reload.
  //
  // One board shape serves every test here:
  //
  //   Doomed column: A, B            Kept column: P (partner), Q, R
  //   A → P, B → P   (P's "before" side is entirely doomed cards)
  //   Q → B          (Q's only link is to a doomed card)
  //   A → B          (both ends doomed)
  //   P → R          (neither end doomed: must survive)
  test.describe('deleting a column that holds linked cards', () => {
    function watchForPanics(page: import('@playwright/test').Page) {
      const panics: string[] = [];
      page.on('console', msg => {
        if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
      });
      page.on('pageerror', err => panics.push(String(err)));
      return panics;
    }

    async function linkedBoard(request: import('@playwright/test').APIRequestContext, slug: string) {
      const board = await apiCreateBoard(request, `${slug}-${Date.now()}`);
      // Positions put the kept column first, so the doomed one is not the
      // board's only or first column.
      const kept = await apiCreateColumn(request, board.name, 'Kept col', 0);
      const doomed = await apiCreateColumn(request, board.name, 'Doomed col', 1);
      const a = await apiCreateCard(request, doomed.id, '# Doomed A');
      const b = await apiCreateCard(request, doomed.id, '# Doomed B');
      const p = await apiCreateCard(request, kept.id, '# Partner card');
      const q = await apiCreateCard(request, kept.id, '# Lonely card');
      const r = await apiCreateCard(request, kept.id, '# Survivor card');
      await apiCreateLink(request, a.id, 'successor', p.id);
      await apiCreateLink(request, b.id, 'successor', p.id);
      await apiCreateLink(request, q.id, 'successor', b.id);
      await apiCreateLink(request, a.id, 'successor', b.id);
      await apiCreateLink(request, p.id, 'successor', r.id);
      return { board, kept, doomed, a, b, p, q, r };
    }

    /** Every assertion that the column's links, and only those, are gone. */
    async function expectOnlySurvivingLinks(
      page: import('@playwright/test').Page,
      request: import('@playwright/test').APIRequestContext,
      b: Awaited<ReturnType<typeof linkedBoard>>
    ) {
      await expect(page.locator('.column-name').filter({ hasText: 'Doomed col' })).toHaveCount(0);
      await expect(cardWith(page, 'Doomed A')).toHaveCount(0);
      // The survivors are all still on screen — asserted positively, so the
      // badge checks below cannot pass merely because the board is empty.
      await expect(page.locator('.card-item')).toHaveCount(3);
      // P loses both of its doomed "before" pills and keeps its "after" one.
      await expect(cardWith(page, 'Partner card').locator('.link-badge-after')).toHaveText([
        `↓#${b.r.number}`,
      ]);
      await expect(cardWith(page, 'Partner card').locator('.link-badge-before')).toHaveCount(0);
      // Q's only link went with B.
      await expect(cardWith(page, 'Lonely card').locator('.link-badge')).toHaveCount(0);
      // R's link to P is untouched.
      await expect(cardWith(page, 'Survivor card').locator('.link-badge')).toHaveText([
        `↑#${b.p.number}`,
      ]);
      // The server agrees: the cascade removed exactly the four doomed links.
      await expect.poll(async () => apiListLinks(request, b.board.name)).toEqual([
        expect.objectContaining({ predecessor_id: b.p.id, successor_id: b.r.id }),
      ]);
    }

    /**
     * The tab still reacts: two repaints a wedged executor cannot make. A
     * query matching no card empties the board — an expanded card included,
     * since a query it fails releases its pin (card #375) — and clearing it
     * brings the three survivors back, all collapsed.
     */
    async function expectLive(page: import('@playwright/test').Page) {
      await page.locator('.navbar-search-input').fill('matches no card at all');
      await expect(page.locator('.card-item')).toHaveCount(0);
      await page.locator('.navbar-search-input').fill('');
      await expect(page.locator('.card-item')).toHaveCount(3);
      await expect(page.locator('.card-item.card-expanded')).toHaveCount(0);
    }

    test('a chooser delete with no broadcast clears the partner cards', async ({
      page,
      request,
    }) => {
      // The local path: `BoardChooser`'s delete, with the stream silenced the
      // way the #313 tests silence it (subscribed to a board nothing publishes
      // to — a dead stream would make the tab refuse the delete outright).
      const b = await linkedBoard(request, 'links-col-del-local');
      const panics = watchForPanics(page);
      await page.route('**/api/events*', route => {
        const url = new URL(route.request().url());
        url.searchParams.set('board_id', 'no-such-board');
        return route.continue({ url: url.toString() });
      });
      await gotoBoardView(page, b.board.name);
      await expect(cardWith(page, 'Partner card').locator('.link-badge-before')).toHaveText([
        `↑#${b.a.number}`,
        `↑#${b.b.number}`,
      ]);

      await openChooser(page);
      page.once('dialog', dialog => dialog.accept());
      await page
        .locator('.chooser-col-row')
        .filter({ hasText: 'Doomed col' })
        .locator('.chooser-col-delete')
        .click();
      await closeChooser(page);

      await expectOnlySurvivingLinks(page, request, b);
      // No chip degraded to a bare `#N`: the partner's editor lists only R.
      await cardWith(page, 'Partner card').click();
      const expanded = page.locator('.card-item.card-expanded');
      await expect(expanded).toHaveCount(1);
      await expect(expanded.locator('.link-group')).toHaveCount(2);
      await expect(expanded.locator('.link-group[data-side="before"] .link-chip')).toHaveCount(0);
      await expect(
        expanded.locator('.link-group[data-side="after"] .link-chip-card')
      ).toHaveText(`#${b.r.number} Survivor card`);
      await expectLive(page);
      expect(panics).toEqual([]);
    });

    test('a lagged tab heals from the column event alone', async ({ page, request }) => {
      // The SSE `ColumnDeleted` arm. A healthy stream has pruned every link
      // through `card_link_deleted` before the column event lands, so only a
      // tab that lost those — a receiver lagged out of the backend's 128-slot
      // broadcast channel — reaches the new code. Simulated exactly as the
      // #313 lagged-card test does: drop `card_link_deleted` on the way in.
      //
      // The delete is remote (the API), so the chooser path is not involved,
      // and the partner is left **expanded** so the prune notifies its live
      // `LinkChip`s and `LinkBadges` from the SSE handler — the disposal-trap
      // shape iteration 54 found — while the whole doomed column unmounts.
      const b = await linkedBoard(request, 'links-col-del-lagged');
      const panics = watchForPanics(page);
      await page.addInitScript(() => {
        const Native = window.EventSource;
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        (window as any).EventSource = function (...args: unknown[]) {
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          const es = new (Native as any)(...args);
          Object.defineProperty(es, 'onmessage', {
            set(handler: (ev: MessageEvent) => void) {
              Native.prototype.addEventListener.call(es, 'message', (ev: Event) => {
                const msg = ev as MessageEvent;
                try {
                  if (JSON.parse(msg.data)?.type === 'card_link_deleted') return;
                } catch {
                  /* not JSON — pass it through untouched */
                }
                handler(msg);
              });
            },
            configurable: true,
          });
          return es;
        };
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        (window as any).EventSource.prototype = Native.prototype;
      });
      const eventsReady = page.waitForResponse(
        response =>
          response.request().method() === 'GET' &&
          response.url().includes(`/api/events?board_id=${b.board.id}`) &&
          response.ok()
      );
      await gotoBoardView(page, b.board.name);
      await eventsReady;

      await cardWith(page, 'Partner card').click();
      const expanded = page.locator('.card-item.card-expanded');
      await expect(
        expanded.locator('.link-group[data-side="before"] .link-chip-card')
      ).toHaveText([`#${b.a.number} Doomed A`, `#${b.b.number} Doomed B`]);

      await apiDeleteColumn(request, b.doomed.id);

      // The column goes over SSE, and its cards' links with it — this tab
      // never saw a single `card_link_deleted`.
      await expect(page.locator('.column-name').filter({ hasText: 'Doomed col' })).toHaveCount(0, {
        timeout: 5000,
      });
      await expect(expanded).toHaveCount(1);
      await expect(expanded.locator('.link-group')).toHaveCount(2);
      await expect(expanded.locator('.link-group[data-side="before"] .link-chip')).toHaveCount(0);
      await expect(
        expanded.locator('.link-group[data-side="after"] .link-chip-card')
      ).toHaveText(`#${b.r.number} Survivor card`);
      // The liveness check also lets go of the expanded partner (a query it
      // fails releases the pin), so the collapsed badges can be read after.
      await expectLive(page);
      await expectOnlySurvivingLinks(page, request, b);
      expect(panics).toEqual([]);
    });

    test('a healthy stream deletes a linked column cleanly', async ({ page, request }) => {
      // The regression guard for the two above: with SSE working, the link
      // events, the column event and the chooser's own delete all prune the
      // same links, and the column unmounting between those passes must not
      // panic. (That the later passes write nothing is pinned by the host test
      // `a_prune_that_finds_nothing_notifies_no_link_reader`, not here: this
      // end state is the same either way.)
      const b = await linkedBoard(request, 'links-col-del-sse-ok');
      const panics = watchForPanics(page);
      const eventsReady = page.waitForResponse(
        response =>
          response.request().method() === 'GET' &&
          response.url().includes(`/api/events?board_id=${b.board.id}`) &&
          response.ok()
      );
      await gotoBoardView(page, b.board.name);
      await eventsReady;
      await expect(cardWith(page, 'Partner card').locator('.link-badge-before')).toHaveCount(2);

      await openChooser(page);
      page.once('dialog', dialog => dialog.accept());
      await page
        .locator('.chooser-col-row')
        .filter({ hasText: 'Doomed col' })
        .locator('.chooser-col-delete')
        .click();
      await closeChooser(page);

      await expectOnlySurvivingLinks(page, request, b);
      await expectLive(page);
      expect(panics).toEqual([]);
    });

    test('deleting the board being viewed navigates away without a panic', async ({
      page,
      request,
    }) => {
      // The board half of the card needed no fix, and this test does **not**
      // prove that — no e2e can: every card at either end of the deleted
      // board's links goes with it, and the server refuses cross-board links,
      // so no card on any other board could ever show one of them, stale
      // index or not. The "no bug" verdict rests on the code: links are
      // board-scoped, and deleting the board on screen always navigates, which
      // re-runs `BoardView`'s load effect and empties `board_links` first.
      //
      // What this does check is the unmount the delete causes, with linked
      // cards on screen and the stream silenced as above: no disposal panic,
      // and a tab that still reacts afterwards.
      const b = await linkedBoard(request, 'links-board-del');
      // Somewhere to land that is not the deleted board.
      const landing = await apiCreateBoard(request, `links-board-del-landing-${Date.now()}`);
      const panics = watchForPanics(page);
      await page.route('**/api/events*', route => {
        const url = new URL(route.request().url());
        url.searchParams.set('board_id', 'no-such-board');
        return route.continue({ url: url.toString() });
      });
      await gotoBoardView(page, b.board.name);
      await expect(cardWith(page, 'Partner card').locator('.link-badge-before')).toHaveCount(2);

      await openChooser(page);
      page.once('dialog', dialog => dialog.accept());
      await page
        .locator('.chooser-board-row')
        .filter({ hasText: b.board.name })
        .locator('.chooser-board-delete')
        .click();

      await expect(page).not.toHaveURL(`/boards/${b.board.name}`);
      await page.waitForSelector('.columns-row');
      // Liveness: the chooser opens again and lists the boards as they now
      // are — two repaints a wedged executor could not make.
      await openChooser(page);
      await expect(
        page.locator('.chooser-board-row').filter({ hasText: landing.name })
      ).toHaveCount(1);
      await expect(
        page.locator('.chooser-board-row').filter({ hasText: b.board.name })
      ).toHaveCount(0);
      expect(panics).toEqual([]);
    });
  });

  test('a healthy stream clears a deleted card\'s links exactly once', async ({
    page,
    request,
  }) => {
    // The regression guard for the above: with SSE working, the local prune and
    // the broadcast `CardLinkDeleted` both run over the same link. Removing an
    // absent link must be a no-op, not an error — and the partner's surviving
    // link must not be swept up by the second pass either.
    const board = await apiCreateBoard(request, `links-del-sse-ok-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Todo');
    const doomed = await apiCreateCard(request, col.id, '# Doomed card');
    const partner = await apiCreateCard(request, col.id, '# Partner card');
    const other = await apiCreateCard(request, col.id, '# Third card');
    await apiCreateLink(request, doomed.id, 'successor', partner.id);
    await apiCreateLink(request, partner.id, 'successor', other.id);

    // Only reactive-runtime failures, not incidental console noise: a double
    // removal would surface as a disposal panic, which is what this guards.
    const panics: string[] = [];
    page.on('console', msg => {
      if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
    });
    page.on('pageerror', err => panics.push(String(err)));

    const eventsReady = page.waitForResponse(
      response =>
        response.request().method() === 'GET' &&
        response.url().includes(`/api/events?board_id=${board.id}`) &&
        response.ok()
    );
    await gotoBoardView(page, board.name);
    await eventsReady;

    await cardWith(page, 'Doomed card').click();
    await page.locator('.card-toolbar-close').first().click();
    await page.locator('.btn-danger').click();

    await expect(cardWith(page, 'Doomed card')).toHaveCount(0);
    await expect(cardWith(page, 'Partner card').locator('.link-badge-before')).toHaveCount(0);
    await expect(cardWith(page, 'Partner card').locator('.link-badge-after')).toHaveText(
      `↓#${other.number}`
    );
    // Give the broadcast time to land on top of the local prune, then confirm
    // the double pass changed nothing and raised nothing.
    await expect.poll(async () => apiListLinks(request, board.name)).toEqual([
      expect.objectContaining({ predecessor_id: partner.id, successor_id: other.id }),
    ]);
    await expect(cardWith(page, 'Partner card').locator('.link-badge-after')).toHaveText(
      `↓#${other.number}`
    );
    expect(panics).toEqual([]);
  });

  // Card #369. `LinkPicker` sits beside the link components iteration 54
  // hardened, but no reproduction then had its input focused and its popup
  // open — so its closures were never subscribed at the moment the card was
  // unmounted. These open it first, then take the card away.
  test.describe('an open link picker survives its card being unmounted', () => {
    function watchForPanics(page: import('@playwright/test').Page) {
      const panics: string[] = [];
      page.on('console', msg => {
        if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
      });
      page.on('pageerror', err => panics.push(String(err)));
      return panics;
    }

    /** Expand `text`'s card and type into its "after" picker until `match` is offered. */
    async function openPicker(page: import('@playwright/test').Page, text: string, match: string) {
      await cardWith(page, text).click();
      const input = page.locator('.card-item.card-expanded .link-group[data-side="after"] .link-picker-input');
      await input.fill(match);
      await expect(input).toBeFocused();
      await expect(page.locator('.link-suggestions .link-suggestion')).toHaveCount(1);
    }

    test('deleted elsewhere while the picker is focused', async ({ page, request }) => {
      // The one path that keeps the input focused, and so the popup subscribed,
      // right up to the unmount: the delete arrives over SSE, not from a click.
      const board = await apiCreateBoard(request, `links-picker-del-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      const doomed = await apiCreateCard(request, col.id, '# Doomed card');
      await apiCreateCard(request, col.id, '# Target card');

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await openPicker(page, 'Doomed card', 'target');

      await apiDeleteCard(request, doomed.id);
      await expect(cardWith(page, 'Doomed card')).toHaveCount(0);
      await expect(page.locator('.link-suggestions')).toHaveCount(0);

      // Liveness: a wedged executor would leave this click inert.
      await cardWith(page, 'Target card').click();
      await expect(page.locator('.card-item.card-expanded')).toHaveCount(1);
      await apiCreateCard(request, col.id, '# Arrived later');
      await expect(cardWith(page, 'Arrived later')).toHaveCount(1);
      expect(panics).toEqual([]);
    });

    test('unmounted by a search typed past it (liveness only)', async ({ page, request }) => {
      // A query change releases the expanded-card pin when the card fails the
      // new query (#375), so the search filter unmounts the picker's card.
      //
      // **Not a #369 regression guard.** Filling the search box blurs the
      // picker input first, which closes the popup, so the converted
      // `popup_open`/`suggestions` closures are not subscribed when the card
      // goes — this passes with those conversions reverted. It is kept as a
      // liveness check on this unmount path; the remote-delete test above is
      // the only one here that reaches the fix.
      const board = await apiCreateBoard(request, `links-picker-lock-${Date.now()}`);
      const col = await apiCreateColumn(request, board.name, 'Todo');
      await apiCreateCard(request, col.id, '# Pinned card');
      await apiCreateCard(request, col.id, '# Target card');
      await apiCreateCard(request, col.id, '# Other match');

      const panics = watchForPanics(page);
      await gotoBoardView(page, board.name);
      await openPicker(page, 'Pinned card', 'target');
      // Search for something the pinned card does not contain; it stays only
      // because it is expanded. Filling the box also blurs the picker input.
      await page.locator('.navbar-search-input').fill('other');
      await expect(cardWith(page, 'Pinned card')).toHaveCount(0);
      await expect(cardWith(page, 'Other match')).toHaveCount(1);

      // Liveness.
      await cardWith(page, 'Other match').click();
      await expect(page.locator('.card-item.card-expanded')).toHaveCount(1);
      await page.locator('.navbar-search-input').fill('');
      await expect(page.locator('.card-item')).toHaveCount(3);
      expect(panics).toEqual([]);
    });
  });
});
