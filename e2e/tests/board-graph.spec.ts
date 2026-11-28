import { test, expect, Page, APIRequestContext } from '@playwright/test';
import {
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiCreateLink,
  apiDeleteCard,
  gotoBoardView,
} from './helpers';

// Board link graph — iteration 86 / card #303.
//
// Graph mode (`/boards/:slug/graph`) shows every card of a visible column as a
// node under its column's header and every link between them as a line with
// an arrowhead at the *after* end. It is a mode of the board page: flipping it
// keeps the board mounted, and the graph follows the board's live state.
//
// Nodes are `.graph-node[data-card-number]`; lines are
// `.graph-edge[data-from][data-to]` (card numbers); reason labels are
// `.graph-edge-label[data-link-id]`.
//
// `apiCreateCard` inserts at the top of its column, so cards are created in
// reverse of the order a column should read.

/** Collect panics and disposal traps, which wedge the tab without failing a click. */
function watchForPanics(page: Page) {
  const panics: string[] = [];
  page.on('console', (msg) => {
    if (/panic|already been disposed/i.test(msg.text())) panics.push(msg.text());
  });
  page.on('pageerror', (err) => panics.push(String(err)));
  return panics;
}

/** Open a board straight in graph mode and wait for the graph body. */
async function gotoGraph(page: Page, slug: string) {
  await page.goto(`/boards/${slug}/graph`);
  await expect(page.locator('.graph-body')).toBeVisible();
}

const node = (page: Page, number: number) =>
  page.locator(`.graph-node[data-card-number="${number}"]`);

const edge = (page: Page, from: number, to: number) =>
  page.locator(`.graph-edge[data-from="${from}"][data-to="${to}"]`);

/**
 * The app still responds after whatever the test just did: the search box is
 * a two-way binding, so typing into it and clearing it with Escape only works
 * while the reactive runtime is alive. A panic or a wedged effect fails here
 * even when the action under test appeared to succeed.
 */
async function expectAlive(page: Page) {
  const search = page.locator('.navbar-search-input');
  await search.fill('alive');
  await expect(search).toHaveValue('alive');
  await search.press('Escape');
  await expect(search).toHaveValue('');
}

/** Todo / Doing / Done with a few linked cards, as most tests here need. */
async function linkedBoard(request: APIRequestContext, slug: string) {
  const board = await apiCreateBoard(request, `${slug}-${Date.now()}`);
  const todo = await apiCreateColumn(request, board.name, 'Todo', 0);
  const doing = await apiCreateColumn(request, board.name, 'Doing', 1);
  const done = await apiCreateColumn(request, board.name, 'Done', 2);
  const later = await apiCreateCard(request, todo.id, '# Later work');
  const next = await apiCreateCard(request, todo.id, '# Next work');
  const active = await apiCreateCard(request, doing.id, '# Active work');
  const shipped = await apiCreateCard(request, done.id, '# Shipped work');
  const loose = await apiCreateCard(request, done.id, '# Loose end');
  // Done → Doing → Todo, and down the Todo column.
  const shippedToActive = await apiCreateLink(request, shipped.id, 'successor', active.id);
  const activeToNext = await apiCreateLink(request, active.id, 'successor', next.id);
  const nextToLater = await apiCreateLink(
    request,
    next.id,
    'successor',
    later.id,
    'needs the API first'
  );
  return {
    board,
    columns: { todo, doing, done },
    cards: { later, next, active, shipped, loose },
    links: { shippedToActive, activeToNext, nextToLater },
  };
}

test.describe('board link graph', () => {
  test('renders a node per card and an edge per link', async ({ page, request }) => {
    const { board, cards } = await linkedBoard(request, 'graph-render');
    const panics = watchForPanics(page);

    await gotoGraph(page, board.name);

    await expect(page.locator('.graph-node')).toHaveCount(5);
    for (const card of Object.values(cards)) {
      await expect(node(page, card.number)).toBeVisible();
    }
    // `#N` and the title from the body's first line.
    await expect(node(page, cards.active.number).locator('.graph-node-number')).toHaveText(
      `#${cards.active.number}`
    );
    await expect(node(page, cards.active.number).locator('.graph-node-title')).toHaveText(
      'Active work'
    );

    await expect(page.locator('.graph-edge')).toHaveCount(3);
    await expect(edge(page, cards.shipped.number, cards.active.number)).toHaveCount(1);
    await expect(edge(page, cards.active.number, cards.next.number)).toHaveCount(1);
    await expect(edge(page, cards.next.number, cards.later.number)).toHaveCount(1);

    // Within a column the link reads downward: `next` sits above `later`
    // even though text mode lists `later` first.
    const nextBox = await node(page, cards.next.number).boundingBox();
    const laterBox = await node(page, cards.later.number).boundingBox();
    expect(nextBox!.y).toBeLessThan(laterBox!.y);

    // Each node sits under its own column's header.
    for (const [card, column] of [
      [cards.next, 'Todo'],
      [cards.active, 'Doing'],
      [cards.shipped, 'Done'],
    ] as const) {
      const header = await page
        .locator('.column-view')
        .filter({ has: page.locator('.column-name', { hasText: new RegExp(`^${column}$`, 'i') }) })
        .boundingBox();
      const box = await node(page, card.number).boundingBox();
      expect(box!.x).toBeGreaterThanOrEqual(header!.x);
      expect(box!.x + box!.width).toBeLessThanOrEqual(header!.x + header!.width);
    }

    // The navbar switch says graph mode, and the card lists are hidden.
    await expect(page.getByRole('switch', { name: 'Graph view' })).toHaveAttribute(
      'aria-checked',
      'true'
    );
    await expect(page.locator('.card-item').first()).toBeHidden();

    await expectAlive(page);
    expect(panics).toEqual([]);
  });

  test('clicking a node opens its card, an edit shows in the node, and closing stays in graph mode', async ({
    page,
    request,
  }) => {
    const { board, cards } = await linkedBoard(request, 'graph-open');
    const panics = watchForPanics(page);
    await gotoGraph(page, board.name);

    await node(page, cards.active.number).click();
    await expect(page).toHaveURL(
      new RegExp(`/boards/${board.name}/graph\\?card=${cards.active.number}$`)
    );
    await expect(page.locator('.modal')).toBeVisible();

    // Edit the body in the modal; the save comes back over SSE and retitles
    // the node.
    await page.locator('.modal-body-rendered').click();
    await page.locator('.modal-body-textarea').fill('# Active work, renamed');
    await expect(node(page, cards.active.number).locator('.graph-node-title')).toHaveText(
      'Active work, renamed'
    );

    // Closing returns to the graph, not to text mode. Closed with Escape:
    // the toolbar's "Restore to board" button raises a wasm-bindgen
    // "closure invoked recursively or after being dropped" page error on
    // every close, in text mode too and on main before this card — the
    // click bubbles into the modal's own handler after the close has
    // dropped it. That is not this card's to fix, and it would trip the
    // panic watch here.
    await page.locator('.modal').press('Escape');
    await expect(page.locator('.modal')).toHaveCount(0);
    await expect(page).toHaveURL(new RegExp(`/boards/${board.name}/graph$`));
    await expect(page.locator('.graph-body')).toBeVisible();
    await expect(node(page, cards.active.number)).toBeVisible();

    await expectAlive(page);
    expect(panics).toEqual([]);
  });

  test('a link added elsewhere appears in the graph without a reload', async ({
    page,
    request,
  }) => {
    const { board, cards } = await linkedBoard(request, 'graph-live-link');
    const panics = watchForPanics(page);
    await gotoGraph(page, board.name);
    await expect(page.locator('.graph-edge')).toHaveCount(3);
    // A marker a reload would wipe.
    await page.evaluate(() => ((window as any).__graphNoReload = true));

    await apiCreateLink(request, cards.loose.id, 'successor', cards.later.id);

    await expect(edge(page, cards.loose.number, cards.later.number)).toHaveCount(1);
    await expect(page.locator('.graph-edge')).toHaveCount(4);
    expect(await page.evaluate(() => (window as any).__graphNoReload)).toBe(true);

    await expectAlive(page);
    expect(panics).toEqual([]);
  });

  test('deleting a card removes its node and its edges', async ({ page, request }) => {
    const { board, cards } = await linkedBoard(request, 'graph-delete');
    const panics = watchForPanics(page);
    await gotoGraph(page, board.name);
    await expect(edge(page, cards.active.number, cards.next.number)).toHaveCount(1);

    await apiDeleteCard(request, cards.active.id);

    await expect(node(page, cards.active.number)).toHaveCount(0);
    await expect(page.locator(`.graph-edge[data-from="${cards.active.number}"]`)).toHaveCount(0);
    await expect(page.locator(`.graph-edge[data-to="${cards.active.number}"]`)).toHaveCount(0);
    // The link that did not touch it stays.
    await expect(edge(page, cards.next.number, cards.later.number)).toHaveCount(1);
    await expect(page.locator('.graph-node')).toHaveCount(4);

    await expectAlive(page);
    expect(panics).toEqual([]);
  });

  test('a board with no links renders its cards without error', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `graph-nolinks-${Date.now()}`);
    const todo = await apiCreateColumn(request, board.name, 'Todo', 0);
    const done = await apiCreateColumn(request, board.name, 'Done', 1);
    const second = await apiCreateCard(request, todo.id, '# Second');
    const first = await apiCreateCard(request, todo.id, '# First');
    await apiCreateCard(request, done.id, '# Finished');
    const panics = watchForPanics(page);

    await gotoGraph(page, board.name);

    await expect(page.locator('.graph-node')).toHaveCount(3);
    await expect(page.locator('.graph-edge')).toHaveCount(0);
    // Unlinked cards keep their column order: two to a row, left to right.
    const firstBox = await node(page, first.number).boundingBox();
    const secondBox = await node(page, second.number).boundingBox();
    expect(firstBox!.x).toBeLessThan(secondBox!.x);
    expect(firstBox!.y).toBe(secondBox!.y);

    await expectAlive(page);
    expect(panics).toEqual([]);
  });

  test('the switch round-trips text and graph mode, and a reload keeps graph mode', async ({
    page,
    request,
  }) => {
    const { board, cards } = await linkedBoard(request, 'graph-toggle');
    const panics = watchForPanics(page);
    await gotoBoardView(page, board.name);
    const toggle = page.getByRole('switch', { name: 'Graph view' });
    await expect(toggle).toHaveAttribute('aria-checked', 'false');
    await expect(page.locator('.card-item')).toHaveCount(5);

    // A search typed in text mode survives the flip: the board view is not
    // remounted, only its mode changes.
    const search = page.locator('.navbar-search-input');
    await search.fill('work');

    await toggle.click();
    await expect(page).toHaveURL(new RegExp(`/boards/${board.name}/graph$`));
    await expect(toggle).toHaveAttribute('aria-checked', 'true');
    await expect(page.locator('.graph-body')).toBeVisible();
    await expect(node(page, cards.active.number)).toBeVisible();
    await expect(page.locator('.card-item').first()).toBeHidden();
    await expect(search).toHaveValue('work');

    await toggle.click();
    await expect(page).toHaveURL(new RegExp(`/boards/${board.name}$`));
    await expect(toggle).toHaveAttribute('aria-checked', 'false');
    await expect(page.locator('.graph-body')).toHaveCount(0);
    await expect(page.locator('.card-item').first()).toBeVisible();
    await search.fill('');

    // Straight to the graph URL, then a reload: still graph mode.
    await gotoGraph(page, board.name);
    await page.reload();
    await expect(page.locator('.graph-body')).toBeVisible();
    await expect(toggle).toHaveAttribute('aria-checked', 'true');
    await expect(page.locator('.graph-node')).toHaveCount(5);

    await expectAlive(page);
    expect(panics).toEqual([]);
  });

  test('the column headers follow the graph when it scrolls sideways', async ({
    page,
    request,
  }) => {
    // Three 370px-minimum columns do not fit 900px, so the graph body
    // scrolls sideways; the header row above it must move with it.
    await page.setViewportSize({ width: 900, height: 800 });
    const { board, cards } = await linkedBoard(request, 'graph-hscroll');
    const panics = watchForPanics(page);
    await gotoGraph(page, board.name);
    await expect(page.locator('.graph-node')).toHaveCount(5);

    const body = page.locator('.graph-body');
    expect(await body.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeGreaterThan(100);
    await body.evaluate((el) => {
      el.scrollLeft = 100;
    });
    await expect
      .poll(() => page.locator('.columns-row').evaluate((el) => el.scrollLeft))
      .toBe(100);

    // Still lined up: the Done card sits under the Done header.
    const header = await page
      .locator('.column-view')
      .filter({ has: page.locator('.column-name', { hasText: /^Done$/i }) })
      .boundingBox();
    const box = await node(page, cards.shipped.number).boundingBox();
    expect(box!.x).toBeGreaterThanOrEqual(header!.x);
    expect(box!.x + box!.width).toBeLessThanOrEqual(header!.x + header!.width);

    await expectAlive(page);
    expect(panics).toEqual([]);
  });

  test('a link reason is shown on its edge', async ({ page, request }) => {
    const { board, links } = await linkedBoard(request, 'graph-reason');
    await gotoGraph(page, board.name);

    const label = page.locator(`.graph-edge-label[data-link-id="${links.nextToLater.id}"]`);
    await expect(label).toHaveText('needs the API first');
    // Only the link with a reason has a label.
    await expect(page.locator('.graph-edge-label')).toHaveCount(1);
    await expectAlive(page);
  });

  test('collapsing a column hides its nodes and their edges', async ({ page, request }) => {
    const { board, cards } = await linkedBoard(request, 'graph-collapse');
    const panics = watchForPanics(page);
    await gotoGraph(page, board.name);
    await expect(page.locator('.graph-edge')).toHaveCount(3);

    const doing = page
      .locator('.column-view')
      .filter({ has: page.locator('.column-name', { hasText: /^Doing$/i }) });
    await doing.locator('.column-collapse-btn').click();

    await expect(node(page, cards.active.number)).toHaveCount(0);
    // Both of the Doing card's links go; the Todo link stays.
    await expect(page.locator('.graph-edge')).toHaveCount(1);
    await expect(edge(page, cards.next.number, cards.later.number)).toHaveCount(1);
    await expect(page.locator('.graph-node')).toHaveCount(4);
    // The collapsed column's strip names it.
    await expect(page.locator('.graph-band-collapsed .graph-band-name')).toHaveText(/^doing$/i);

    // And expanding brings them back.
    await doing.locator('.column-expand-btn').click();
    await expect(node(page, cards.active.number)).toBeVisible();
    await expect(page.locator('.graph-edge')).toHaveCount(3);

    await expectAlive(page);
    expect(panics).toEqual([]);
  });
});
