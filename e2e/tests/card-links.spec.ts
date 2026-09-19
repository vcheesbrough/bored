import { test, expect } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiCreateLink,
  apiDeleteCard,
  apiListLinks,
  apiMoveCard,
  gotoBoardView,
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

  // ── Deleting a linked card with the SSE stream down — card #313 ──────────
  //
  // The board prunes its link index locally now instead of waiting for the
  // server's `CardLinkDeleted` events. These tests hold the browser to that by
  // taking the stream away entirely: `/api/events` is aborted before the board
  // loads, so nothing the server broadcasts can reach the page and every
  // assertion below is about what the tab did for itself. That also stands in
  // for the subtler real-world case — a receiver lagging out of the backend's
  // 128-slot broadcast channel — which drops events the same way but is not
  // reproducible on demand.
  test.describe('deleting a linked card with SSE down', () => {
    /** Abort `/api/events` so the page never receives a single broadcast. */
    async function killSse(page: import('@playwright/test').Page) {
      await page.route('**/api/events*', route => route.abort());
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
      await killSse(page);
      await gotoBoardView(page, board.name);
      await expect(cardWith(page, 'Partner card').locator('.link-badge-before')).toHaveText(
        `↑#${doomed.number}`
      );

      await cardWith(page, 'Doomed card').click();
      await page.locator('.card-toolbar-close').first().click();
      await page.locator('.btn-danger').click();

      // The card goes, and so does its link — with no reload and no SSE.
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
      await killSse(page);
      await gotoBoardView(page, board.name);
      await cardWith(page, 'Doomed card').click();
      await page.locator('.card-toolbar-btn[title="Maximise"]').click();
      await expect(page.locator('.modal')).toBeVisible();

      await page.locator('.modal .card-toolbar-close').first().click();
      await page.locator('.btn-danger').click();

      // `on_modal_delete` used to be a no-op, so with no SSE both the card and
      // its link survived here.
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

    // The tab still reacts — a wedged executor would repaint nothing. The
    // partner is still expanded, so card #304's pin holds it in the filtered
    // view whatever the query says; a query matching neither card therefore
    // leaves exactly that one, and the third card disappearing is what proves
    // the filter re-ran.
    await page.locator('.navbar-search-input').fill('matches no card at all');
    await expect(page.locator('.card-item')).toHaveCount(1);
    await expect(page.locator('.card-item.card-expanded')).toHaveCount(1);

    await context.close();
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
});
