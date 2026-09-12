import { test, expect } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiCreateLink,
  apiListLinks,
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

    // The chip appears on Alpha's "after" side and the link round-trips.
    await expect(after.locator('.link-chip-card')).toHaveText(`#${b.number} Beta card`);
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
});
