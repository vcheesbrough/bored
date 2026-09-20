import { test, expect, Page, Locator } from '@playwright/test';
import { apiCreateBoard, apiCreateColumn, apiCreateCard, gotoBoardView } from './helpers';

/**
 * Card #382 — clicking rendered markdown puts the textarea caret on the
 * character that was clicked, and scrolls so it lands under the pointer.
 *
 * Every marker word is wrapped in `**…**` so it renders as its own `<strong>`,
 * which makes it a single annotated run the test can aim at precisely.
 */
const LINE_COUNT = 120;

function buildBody(): string {
  const lines: string[] = [];
  for (let i = 0; i < LINE_COUNT; i++) {
    lines.push(
      `Line ${String(i).padStart(3, '0')} lorem ipsum dolor sit amet consectetur ` +
      `**marker${i}** adipiscing elit sed do eiusmod tempor incididunt ut labore.`
    );
  }
  return lines.join('\n\n');
}

const BODY = buildBody();

/** Click the centre of `target` and return the viewport point that was clicked. */
async function clickCentre(page: Page, target: Locator): Promise<{ x: number; y: number }> {
  await target.scrollIntoViewIfNeeded();
  const box = await target.boundingBox();
  if (!box) throw new Error('target has no bounding box');
  const point = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
  await page.mouse.click(point.x, point.y);
  return point;
}

/** The `<strong>` holding exactly `markerN`, inside `scope`. */
function marker(page: Page, scope: string, index: number): Locator {
  return page.locator(`${scope} strong`).filter({ hasText: new RegExp(`^marker${index}$`) });
}

/**
 * The source offset recorded on the annotated run inside `el`, checked against
 * the markdown so a wrong attribute fails here rather than confusing the caret
 * assertion that follows.
 */
async function runOffset(el: Locator, word: string): Promise<number> {
  const attr = await el.locator('[data-src]').first().getAttribute('data-src');
  expect(attr, 'the run must carry a source offset').not.toBeNull();
  const offset = Number(attr);
  expect(BODY.slice(offset, offset + word.length)).toBe(word);
  return offset;
}

/** Open a card's maximised modal, with the rendered body showing. */
async function openModal(page: Page) {
  await page.locator('.card-item').first().click();
  await page.locator('[title="Maximise"]').first().click();
  await expect(page.locator('.modal-backdrop')).toBeVisible();
  await expect(page.locator('.modal-markdown')).toBeVisible();
}

test.describe('Click-to-edit caret placement', () => {
  test('modal: caret lands on the clicked word and scrolls under the pointer', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `caret-modal-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, BODY);
    await gotoBoardView(page, board.name);
    await openModal(page);

    const word = 'marker60';
    const target = marker(page, '.modal-markdown', 60);
    const offset = await runOffset(target, word);
    const clicked = await clickCentre(page, target);

    const textarea = page.locator('.modal-body-textarea');
    await expect(textarea).toBeVisible();

    // The caret is inside the word that was clicked — not at offset 0, and not
    // merely at the start of the paragraph.
    const selectionStart = await textarea.evaluate((el: HTMLTextAreaElement) => el.selectionStart);
    expect(selectionStart).toBeGreaterThanOrEqual(offset);
    expect(selectionStart).toBeLessThanOrEqual(offset + word.length);

    // The textarea must actually be scrollable, or the scroll assertion below
    // would pass for the wrong reason.
    const geom = await textarea.evaluate((el: HTMLTextAreaElement, pos: number) => {
      const style = getComputedStyle(el);
      const lineHeight = parseFloat(style.lineHeight);
      const paddingBottom = parseFloat(style.paddingBottom);
      // Measure the caret's line independently of the app: shorten the value,
      // read the height that produces, put it back.
      const value = el.value;
      const savedScroll = el.scrollTop;
      const prefix = value.slice(0, pos);
      el.value = prefix.endsWith('\n') ? `${prefix}.` : prefix;
      const prefixHeight = el.scrollHeight;
      el.value = value;
      el.scrollTop = savedScroll;
      const rect = el.getBoundingClientRect();
      const caretTop = Math.max(0, prefixHeight - paddingBottom - lineHeight);
      return {
        caretClientY: rect.top + caretTop - el.scrollTop,
        lineHeight,
        scrollTop: el.scrollTop,
        scrollHeight: el.scrollHeight,
        clientHeight: el.clientHeight,
      };
    }, selectionStart);

    expect(geom.scrollHeight, 'body must be long enough to scroll').toBeGreaterThan(geom.clientHeight);
    expect(geom.scrollTop, 'textarea must have scrolled to reach the caret').toBeGreaterThan(0);
    expect(
      Math.abs(geom.caretClientY - clicked.y),
      `caret at ${geom.caretClientY} should be near the click at ${clicked.y}`
    ).toBeLessThanOrEqual(2 * geom.lineHeight);

    // Liveness: the editor still works after the caret was placed — typing at
    // the caret reaches the server rather than the textarea being left inert.
    await page.keyboard.type('XYZZY');
    // Scoped to the modal: the expanded card behind it has a save icon too.
    await expect(page.locator('.modal-toolbar .card-save-icon')).toHaveText('💾');
    await page.locator('[title="Restore to board"]').click();
    await expect(page.locator('.modal-backdrop')).not.toBeVisible();
    await page.reload();
    await page.waitForSelector('.columns-row');
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-markdown').first()).toContainText('XYZZY');
  });

  test('inline card: caret lands on the clicked word', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `caret-inline-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, BODY);
    await gotoBoardView(page, board.name);

    // Expand in place — no modal.
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-item.card-expanded')).toBeVisible();

    const word = 'marker40';
    const target = marker(page, '.card-markdown', 40);
    const offset = await runOffset(target, word);
    const clicked = await clickCentre(page, target);

    const textarea = page.locator('.card-body-textarea').first();
    await expect(page.locator('.card-item.card-editing')).toBeVisible();

    const selectionStart = await textarea.evaluate((el: HTMLTextAreaElement) => el.selectionStart);
    expect(selectionStart).toBeGreaterThanOrEqual(offset);
    expect(selectionStart).toBeLessThanOrEqual(offset + word.length);

    // The inline textarea is `field-sizing: content` and never scrolls itself,
    // so reaching the caret is entirely the column's job. Without this the
    // column scrolled to the *end* of the card instead of to the caret, and
    // nothing failed — `selectionStart` above is blind to where the column sat.
    const geom = await textarea.evaluate((el: HTMLTextAreaElement, pos: number) => {
      const style = getComputedStyle(el);
      const lineHeight = parseFloat(style.lineHeight);
      const paddingBottom = parseFloat(style.paddingBottom);
      // Find the column that scrolls, so its position can be put back: the
      // measurement below resizes the card and would otherwise disturb it.
      let scroller: HTMLElement | null = el.parentElement;
      while (scroller) {
        const oy = getComputedStyle(scroller).overflowY;
        if (scroller.scrollHeight > scroller.clientHeight && (oy === 'auto' || oy === 'scroll')) break;
        scroller = scroller.parentElement;
      }
      const scrollerTop = scroller ? scroller.scrollTop : 0;

      const value = el.value;
      const prefix = value.slice(0, pos);
      el.value = prefix.endsWith('\n') ? `${prefix}.` : prefix;
      const prefixHeight = el.scrollHeight;
      el.value = value;
      el.setSelectionRange(pos, pos);
      if (scroller) scroller.scrollTop = scrollerTop;

      const caretTop = Math.max(0, prefixHeight - paddingBottom - lineHeight);
      return {
        caretClientY: el.getBoundingClientRect().top + caretTop - el.scrollTop,
        lineHeight,
        foundScroller: scroller !== null,
      };
    }, selectionStart);

    expect(geom.foundScroller, 'the column must be the thing that scrolls here').toBe(true);
    expect(
      Math.abs(geom.caretClientY - clicked.y),
      `caret at ${geom.caretClientY} should be near the click at ${clicked.y}`
    ).toBeLessThanOrEqual(3 * geom.lineHeight);
  });

  test('modal: clicking below the last paragraph lands at the end, not the start', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `caret-tail-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    // Short body so there is empty space under the text to click into. The
    // trailing rule renders as an `<hr>` and produces no text run, so "end of
    // the last run" is several characters short of "end of the value" — which
    // is where a bare `focus()` would leave the caret. Without that gap this
    // test passes whether or not the caret was placed at all.
    const lastText = 'Second and last paragraph.';
    const shortBody = `First paragraph.\n\n${lastText}\n\n---\n`;
    const lastRunEnd = shortBody.indexOf(lastText) + lastText.length;
    await apiCreateCard(request, col.id, shortBody);
    await gotoBoardView(page, board.name);
    await openModal(page);

    const rendered = page.locator('.modal-body-rendered');
    const box = await rendered.boundingBox();
    if (!box) throw new Error('rendered body has no bounding box');
    // Well below the last line of text, still inside the clickable region.
    await page.mouse.click(box.x + box.width / 2, box.y + box.height - 4);

    const textarea = page.locator('.modal-body-textarea');
    await expect(textarea).toBeVisible();
    const selectionStart = await textarea.evaluate((el: HTMLTextAreaElement) => el.selectionStart);
    // The nearest run is the last paragraph, and the click is past its end.
    expect(selectionStart).toBe(lastRunEnd);
    expect(selectionStart, 'must not merely be the end of the value').not.toBe(shortBody.length);
  });

  test('modal: an empty body still opens the editor at offset 0', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `caret-empty-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, '');
    await gotoBoardView(page, board.name);

    await page.locator('.card-item').first().click();
    await page.locator('[title="Maximise"]').first().click();
    await expect(page.locator('.modal-backdrop')).toBeVisible();

    await page.locator('.modal-body-rendered').click();
    const textarea = page.locator('.modal-body-textarea');
    await expect(textarea).toBeVisible();
    await expect(textarea).toBeFocused();
    expect(await textarea.evaluate((el: HTMLTextAreaElement) => el.selectionStart)).toBe(0);
  });

  test('search highlighting survives the source annotation', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `caret-search-board-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, BODY);
    await gotoBoardView(page, board.name);

    await page.locator('.navbar-search-input').fill('consectetur');
    await page.locator('.card-item').first().click();
    await expect(page.locator('.card-markdown mark.search-hit').first()).toBeVisible();

    // The mark must sit *inside* the offset-carrying span, or the caret
    // resolver would never find an offset for highlighted text.
    const nested = await page
      .locator('.card-markdown mark.search-hit')
      .first()
      .evaluate((el: Element) => el.closest('[data-src]') !== null);
    expect(nested, 'mark must be nested inside the annotated run').toBe(true);
  });
});
