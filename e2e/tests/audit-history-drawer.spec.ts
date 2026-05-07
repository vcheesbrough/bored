import { test, expect } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiDeleteCard,
  apiMoveCard,
  apiUpdateCard,
  gotoBoardView,
} from './helpers';

/**
 * Iteration 23 — polished audit history drawer.
 *
 * These tests assert the *user-facing* polish, not internal IDs:
 *   - Headlines and meta-line strings come from `shared::history` helpers.
 *   - Times render in the user's local timezone (no raw UTC `Z` ISO).
 *   - The «You» rule labels current-user rows correctly when `/api/me`
 *     resolves with a display name.
 *   - The «Show moves» toggle still hides/shows move rows.
 *
 * The unit-level branches of the helpers are covered by `cargo test -p shared`;
 * this file is the integration check that the wiring in `history_panel.rs`
 * passes the right inputs to those helpers and that the resulting strings
 * end up in the DOM.
 */
test.describe('Audit history drawer — polished UX', () => {
  test('rows render polished headlines and meta-line strings', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-polish-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(
      request,
      col.id,
      '# Polished history sample\n\nSome body content here.'
    );
    await gotoBoardView(page, board.name);
    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const row = page.locator(`.history-row[data-entity-id="${card.id}"]`);
    await expect(row).toBeVisible();

    // Headline reflects "world after the change" — uses the body's first
    // markdown heading wrapped in guillemets.
    await expect(row.locator('.history-headline')).toHaveText(
      'Created card «Polished history sample»'
    );

    // Sub-line carries the card number from snapshot_after.
    await expect(row.locator('.history-sub')).toHaveText(`Card #${card.number}`);

    // Meta line: actor and a relative-time fragment, separated by a dot.
    const metaText = (await row.locator('.history-meta-line').innerText()).trim();
    expect(metaText).toMatch(/·/);
    // Fresh row → time should be relative, not "Today" yet.
    expect(metaText).toMatch(/(Just now|\d+ minutes? ago)/);
  });

  test('current user is labelled «You» when /api/me resolves', async ({ page, request }) => {
    // The mock OIDC issuer in docker-compose.test.yml hands out
    // preferred_username = "test-user", which is what /api/me returns and
    // what audit rows record. So at least one row should be labelled «You».
    // When tests run in auth-disabled mode, /api/me also returns a
    // synthetic anonymous user — both `actor_display_name` and
    // `me_name` become "anonymous", and the labeller still resolves to
    // «You» on the first rule, so the assertion holds either way.
    const board = await apiCreateBoard(request, `audit-you-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, '# You-rule sample');
    await gotoBoardView(page, board.name);

    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    // Wait for at least one row to render.
    await expect(page.locator('.history-row').first()).toBeVisible();
    // At least one row's actor should be «You».
    await expect(page.locator('.history-actor', { hasText: 'You' }).first()).toBeVisible();
  });

  test('no row exposes raw UTC ISO or Surreal d\'…\' timestamps', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-tz-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, '# Tz hygiene sample');
    await gotoBoardView(page, board.name);

    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();
    await expect(page.locator('.history-row').first()).toBeVisible();

    const drawerText = (await page.locator('.history-drawer').innerText()).trim();
    // No raw UTC ISO timestamps (e.g. 2026-05-07T17:35:01.123Z) leak through.
    expect(drawerText).not.toMatch(/T\d{2}:\d{2}:\d{2}.*Z/);
    // No leftover Surreal `d'…'` wrappers.
    expect(drawerText).not.toMatch(/d'\d{4}-\d{2}-\d{2}/);
  });

  test('meta-line tooltip carries an absolute datetime ending in a tz suffix', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `audit-tooltip-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, '# Tooltip sample');
    await gotoBoardView(page, board.name);

    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();
    const meta = page.locator('.history-row').first().locator('.history-meta-line');
    await expect(meta).toBeVisible();

    const tooltip = await meta.getAttribute('title');
    expect(tooltip, 'meta-line should have a title attribute').not.toBeNull();
    // YYYY-MM-DD HH:MM, optionally followed by a tz token. Accepts both
    // alphabetic abbreviations (BST, PDT, UTC) and the offset-style strings
    // Intl returns for some IANA zones (GMT-8, GMT+5:30) so devs running
    // the suite under those system timezones don't get a false failure.
    expect(tooltip ?? '').toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}( \S+)?$/);
  });

  test('card body edit surfaces the change in the audit row', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `audit-edit-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, '# Edit visibility\n\nshort body');
    await apiUpdateCard(request, card.id, {
      body: '# Edit visibility\n\nshort body extended for the audit log',
    });

    await gotoBoardView(page, board.name);
    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const updateRow = page
      .locator(`.history-row[data-entity-id="${card.id}"]`)
      .filter({ has: page.locator('.history-badge-update') });
    await expect(updateRow).toBeVisible();
    // Headline still names the card; body delta lives in the sub.
    await expect(updateRow.locator('.history-headline')).toHaveText(
      'Edited card «Edit visibility»'
    );
    // Sub: `Card #N · +XX chars` — exact count varies with the test fixture.
    await expect(updateRow.locator('.history-sub')).toHaveText(
      /^Card #\d+ · \+\d+ chars$/
    );
  });

  test('card title rename surfaces as «Renamed card to …» with old title', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `audit-rename-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, '# Original title');
    await apiUpdateCard(request, card.id, { body: '# Renamed title' });

    await gotoBoardView(page, board.name);
    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    const renameRow = page
      .locator(`.history-row[data-entity-id="${card.id}"]`)
      .filter({ has: page.locator('.history-badge-update') });
    await expect(renameRow).toBeVisible();
    await expect(renameRow.locator('.history-headline')).toHaveText(
      'Renamed card to «Renamed title»'
    );
    await expect(renameRow.locator('.history-sub')).toHaveText(
      /^was «Original title» · Card #\d+$/
    );
  });

  test('badge classes appear for create / update / move / delete operations', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `audit-badges-${Date.now()}`);
    const colA = await apiCreateColumn(request, board.name, 'A');
    const colB = await apiCreateColumn(request, board.name, 'B');
    const card = await apiCreateCard(request, colA.id, '# Badge tour');
    await apiUpdateCard(request, card.id, { body: '# Badge tour edited' });
    await apiMoveCard(request, card.id, colB.id, 0);
    await apiDeleteCard(request, card.id);

    await gotoBoardView(page, board.name);
    await page.locator('.navbar-history-btn').click();
    await expect(page.locator('.history-drawer')).toBeVisible();

    // Move rows are hidden by default — exercise the toggle to see them.
    await page.locator('.history-toggle input[type="checkbox"]').check();

    // Each badge class should appear at least once.
    for (const action of ['create', 'update', 'move', 'delete']) {
      await expect(
        page.locator(`.history-badge-${action}`).first(),
        `expected at least one .history-badge-${action} row`
      ).toBeVisible();
    }
  });
});
