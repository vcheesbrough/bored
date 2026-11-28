import { APIRequestContext, Locator, Page, expect, test } from '@playwright/test';
import {
  apiBoardHistory,
  apiCreateBoard,
  apiCreateCard,
  apiCreateColumn,
  apiDeleteCard,
  apiGetCard,
  apiRestoreAudit,
  apiUpdateCard,
  gotoBoardView,
} from './helpers';

// Pasted images (card #67).
//
// A paste is dispatched as a synthetic `ClipboardEvent` whose `clipboardData`
// is a `DataTransfer` holding a real `File`, which is exactly what the SPA's
// handler reads. Playwright cannot drive the browser's own right-click menu,
// so "Open image in new tab" is checked as its parts: the image is a plain
// same-origin `<img>`, nothing cancels its `contextmenu` event, and its `src`
// loads as a page of its own.

/** A real 1×1 transparent PNG (the same bytes as the backend's `PNG_1X1`). */
const PNG_1X1_BASE64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=';

/** An SVG whose script would retitle the document if it ever ran. */
const SVG_WITH_SCRIPT =
  '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">' +
  '<script>document.title = "pwned"</script>' +
  '<rect width="10" height="10" fill="red"/></svg>';

/** The markdown the SPA writes for an uploaded image. */
const IMAGE_MARKDOWN = /!\[image\]\(\/api\/images\/[0-9a-f-]{36}\)/;

/**
 * Paste a file into `target` the way a browser delivers a clipboard image.
 * Returns whether a handler called `preventDefault()`.
 */
async function pasteFile(
  target: Locator,
  file: { base64?: string; text?: string; sizeBytes?: number; name: string; type: string }
): Promise<boolean> {
  return await target.evaluate((element, f) => {
    let parts: BlobPart[];
    if (f.base64 !== undefined) {
      parts = [Uint8Array.from(atob(f.base64), c => c.charCodeAt(0))];
    } else if (f.text !== undefined) {
      parts = [f.text];
    } else {
      // A large file of zeros, built in the page so it never crosses the
      // Playwright protocol.
      parts = [new Uint8Array(f.sizeBytes ?? 0)];
    }
    const data = new DataTransfer();
    data.items.add(new File(parts, f.name, { type: f.type }));
    const event = new ClipboardEvent('paste', {
      clipboardData: data,
      bubbles: true,
      cancelable: true,
    });
    element.dispatchEvent(event);
    return event.defaultPrevented;
  }, file);
}

/** Paste plain text (no file) and report whether anything cancelled it. */
async function pasteText(target: Locator, text: string): Promise<boolean> {
  return await target.evaluate((element, t) => {
    const data = new DataTransfer();
    data.setData('text/plain', t);
    const event = new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true });
    element.dispatchEvent(event);
    return event.defaultPrevented;
  }, text);
}

/** Wait until the stored body holds an uploaded image's markdown, and return it. */
async function waitForImageInBody(request: APIRequestContext, cardId: string): Promise<string> {
  await expect
    .poll(async () => (await apiGetCard(request, cardId)).body, { timeout: 15_000 })
    .toMatch(IMAGE_MARKDOWN);
  return (await apiGetCard(request, cardId)).body;
}

/** The `/api/images/{id}` URL in a body. */
function imageUrl(body: string): string {
  const match = body.match(/\/api\/images\/[0-9a-f-]{36}/);
  if (!match) throw new Error(`no image URL in ${JSON.stringify(body)}`);
  return match[0];
}

/** Wait for `img` to finish loading and return its natural width. */
async function loadedWidth(img: Locator): Promise<number> {
  await expect
    .poll(() => img.evaluate((i: HTMLImageElement) => i.complete && i.naturalWidth > 0), {
      timeout: 10_000,
    })
    .toBe(true);
  return await img.evaluate((i: HTMLImageElement) => i.naturalWidth);
}

/** Open the first card on the board in the full-screen modal, in edit mode. */
async function openModalEditor(page: Page): Promise<Locator> {
  await page.locator('.card-item').first().click();
  await page.locator('[title="Maximise"]').first().click();
  await expect(page.locator('.modal-backdrop')).toBeVisible();
  await page.locator('.modal-body-rendered').click();
  const textarea = page.locator('.modal-body-textarea');
  await expect(textarea).toBeVisible();
  return textarea;
}

test.describe('Pasted images (card #67)', () => {
  test('a pasted image uploads, renders, persists, and opens on its own', async ({
    page,
    request,
    context,
  }) => {
    const board = await apiCreateBoard(request, `images-modal-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Before the image');
    await gotoBoardView(page, board.name);

    const textarea = await openModalEditor(page);
    // Caret at the end, then paste.
    await textarea.press('End');
    const prevented = await pasteFile(textarea, {
      base64: PNG_1X1_BASE64,
      name: 'shot.png',
      type: 'image/png',
    });
    expect(prevented).toBe(true);

    // Saved as markdown, next to the text that was there.
    const body = await waitForImageInBody(request, card.id);
    expect(body).toContain('Before the image');
    expect(body).not.toContain('Uploading image');
    const url = imageUrl(body);

    // Liveness: the editor still edits and saves after a paste.
    await textarea.press('End');
    await textarea.pressSequentially(' and after');
    await expect
      .poll(async () => (await apiGetCard(request, card.id)).body, { timeout: 10_000 })
      .toContain('and after');

    // Leave edit mode: the preview shows the image, really loaded through the
    // authenticated route.
    await page.locator('.modal-body-textarea').blur();
    const img = page.locator('.modal-markdown img');
    expect(await loadedWidth(img)).toBe(1);
    // A plain same-origin <img> ...
    expect(await img.evaluate((i: HTMLImageElement) => new URL(i.src).origin)).toBe(
      new URL(page.url()).origin
    );
    // ... and nothing cancels its context menu, so the browser's own menu
    // (with "Open image in new tab") is what a right-click gets.
    const cancelled = await img.evaluate(i => {
      const event = new MouseEvent('contextmenu', { bubbles: true, cancelable: true, button: 2 });
      i.dispatchEvent(event);
      return event.defaultPrevented;
    });
    expect(cancelled).toBe(false);

    // Opening the image's URL as a page of its own (what "Open image in new
    // tab" does) loads the PNG, with the caching headers the card specifies.
    const tab = await context.newPage();
    const response = await tab.goto(url);
    expect(response?.status()).toBe(200);
    const headers = response!.headers();
    expect(headers['content-type']).toBe('image/png');
    expect(headers['cache-control']).toBe('private, max-age=31536000, immutable');
    expect(headers['etag']).toBe(`"${url.split('/').pop()}"`);
    expect(headers['x-content-type-options']).toBe('nosniff');
    await tab.close();

    // Persistence: close the modal, reload, and the image is still there.
    await page.locator('[title="Restore to board"]').click();
    await expect(page.locator('.modal-backdrop')).not.toBeVisible();
    await page.reload();
    await page.waitForSelector('.columns-row');
    expect(await loadedWidth(page.locator('.card-preview img').first())).toBe(1);
  });

  test('right-clicking a preview image keeps the browser menu, not the card menu', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `images-menu-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const upload = await request.post('/api/images', {
      headers: { 'content-type': 'image/png' },
      data: Buffer.from(PNG_1X1_BASE64, 'base64'),
    });
    expect(upload.status()).toBe(201);
    const { url } = (await upload.json()) as { url: string };
    await apiCreateCard(request, col.id, `Title text\n\n![image](${url})`);
    await gotoBoardView(page, board.name);

    const preview = page.locator('.card-preview').first();
    await loadedWidth(preview.locator('img'));
    await preview.locator('img').click({ button: 'right' });
    await expect(page.locator('.card-context-menu')).toHaveCount(0);

    // Control: a right-click on the card's text still opens bored's menu.
    await preview.getByText('Title text').click({ button: 'right' });
    await expect(page.locator('.card-context-menu')).toBeVisible();
  });

  test('pasting into the inline card editor works too', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `images-inline-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Inline');
    await gotoBoardView(page, board.name);

    await page.locator('.card-item').first().click();
    await page.locator('.card-body-rendered').first().click();
    const textarea = page.locator('.card-body-textarea').first();
    await expect(textarea).toBeVisible();
    await textarea.press('End');
    expect(
      await pasteFile(textarea, { base64: PNG_1X1_BASE64, name: 'x.png', type: 'image/png' })
    ).toBe(true);

    await waitForImageInBody(request, card.id);
    await textarea.press('Escape');
    expect(await loadedWidth(page.locator('.card-markdown img').first())).toBe(1);
  });

  test('a scripted SVG renders as an image but never runs as a page', async ({
    page,
    request,
    context,
  }) => {
    const board = await apiCreateBoard(request, `images-svg-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'SVG');
    await gotoBoardView(page, board.name);

    const textarea = await openModalEditor(page);
    await textarea.press('End');
    await pasteFile(textarea, { text: SVG_WITH_SCRIPT, name: 'x.svg', type: 'image/svg+xml' });
    const url = imageUrl(await waitForImageInBody(request, card.id));

    await page.locator('.modal-body-textarea').blur();
    expect(await loadedWidth(page.locator('.modal-markdown img'))).toBe(10);

    // Opened on its own, the document is sandboxed: its script never runs.
    const tab = await context.newPage();
    const response = await tab.goto(url);
    expect(response?.status()).toBe(200);
    expect(response!.headers()['content-type']).toBe('image/svg+xml');
    expect(response!.headers()['content-security-policy']).toBe(
      "default-src 'none'; style-src 'unsafe-inline'; sandbox"
    );
    // Give a script every chance to have run before reading the title.
    await tab.waitForLoadState('load');
    expect(await tab.title()).not.toBe('pwned');
    await tab.close();

    // Control: the same response with its CSP stripped *does* run the script,
    // so the assertion above can fail. Without this, a browser that simply
    // never ran SVG scripts would pass it vacuously.
    const control = await context.newPage();
    await control.route(`**${url}`, async route => {
      const original = await route.fetch();
      const headers = { ...original.headers() };
      delete headers['content-security-policy'];
      await route.fulfill({ response: original, headers });
    });
    await control.goto(url);
    await expect.poll(() => control.title(), { timeout: 5_000 }).toBe('pwned');
    await control.close();
  });

  test('an image over 10 MB is refused before upload and leaves nothing behind', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `images-large-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Large');
    await gotoBoardView(page, board.name);

    const textarea = await openModalEditor(page);
    let uploads = 0;
    page.on('request', r => {
      if (r.method() === 'POST' && r.url().endsWith('/api/images')) uploads += 1;
    });
    await pasteFile(textarea, { sizeBytes: 11 * 1024 * 1024, name: 'big.png', type: 'image/png' });

    await expect(page.locator('.image-upload-error')).toContainText('too large');
    await expect(textarea).not.toHaveValue(/Uploading image/);
    expect(uploads).toBe(0);
    // Liveness: the editor still saves ordinary edits.
    await textarea.press('End');
    await textarea.pressSequentially(' still editable');
    await expect
      .poll(async () => (await apiGetCard(request, card.id)).body, { timeout: 10_000 })
      .toBe('Large still editable');
  });

  test('an upload that finishes after the modal closed still lands in the card', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `images-late-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Late');
    await gotoBoardView(page, board.name);

    // Hold the upload long enough to close the editor first.
    let release: () => void = () => {};
    const held = new Promise<void>(resolve => (release = resolve));
    await page.route('**/api/images', async route => {
      await held;
      await route.continue();
    });

    const textarea = await openModalEditor(page);
    await textarea.press('End');
    await pasteFile(textarea, { base64: PNG_1X1_BASE64, name: 'x.png', type: 'image/png' });
    // Close mid-upload: the modal flushes the body, placeholder and all.
    await page.locator('[title="Restore to board"]').click();
    await expect(page.locator('.modal-backdrop')).not.toBeVisible();
    await expect
      .poll(async () => (await apiGetCard(request, card.id)).body)
      .toContain('Uploading image');

    // Now let the upload finish: the swap is made on the stored card.
    release();
    const body = await waitForImageInBody(request, card.id);
    expect(body).not.toContain('Uploading image');
    expect(body).toContain('Late');
    // The card is still expanded after the modal; a fresh load shows the
    // collapsed preview, from the stored body.
    await page.reload();
    await page.waitForSelector('.columns-row');
    expect(await loadedWidth(page.locator('.card-preview img').first())).toBe(1);
  });

  test('a failure the server reports removes the placeholder and stays with its card', async ({
    page,
    request,
  }) => {
    const board = await apiCreateBoard(request, `images-refused-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Second');
    const first = await apiCreateCard(request, col.id, 'First');
    await gotoBoardView(page, board.name);

    // The server refuses the type (as it would a file that is no image).
    await page.route('**/api/images', route => route.fulfill({ status: 415 }));
    const textarea = await openModalEditor(page);
    await textarea.press('End');
    await pasteFile(textarea, { base64: PNG_1X1_BASE64, name: 'x.png', type: 'image/png' });

    await expect(page.locator('.image-upload-error')).toContainText('supported image');
    await expect(textarea).toHaveValue('First');
    // Nothing of the paste reached the stored card.
    expect((await apiGetCard(request, first.id)).body).toBe('First');

    // The error belongs to that card: opening another one does not show it.
    await page.locator('[title="Restore to board"]').click();
    await expect(page.locator('.modal-backdrop')).not.toBeVisible();
    await page.locator('.card-item').nth(1).click();
    await page.locator('[title="Maximise"]').first().click();
    await expect(page.locator('.modal-backdrop')).toBeVisible();
    await expect(page.locator('.modal-body-rendered')).toContainText('Second');
    await expect(page.locator('.image-upload-error')).toHaveCount(0);
  });

  test('a text paste is left to the browser', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `images-text-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    await apiCreateCard(request, col.id, 'Text');
    await gotoBoardView(page, board.name);

    const textarea = await openModalEditor(page);
    // Not cancelled: the browser performs its normal paste.
    expect(await pasteText(textarea, 'plain words')).toBe(false);
    await expect(textarea).toHaveValue('Text');
    // Control: an image paste on the same textarea is taken over.
    expect(
      await pasteFile(textarea, { base64: PNG_1X1_BASE64, name: 'x.png', type: 'image/png' })
    ).toBe(true);
  });

  test('an image survives cut-and-paste to another board and its first card being deleted', async ({
    page,
    request,
  }) => {
    const boardA = await apiCreateBoard(request, `images-a-${Date.now()}`);
    const colA = await apiCreateColumn(request, boardA.name, 'Column');
    const cardA = await apiCreateCard(request, colA.id, 'Source');
    await gotoBoardView(page, boardA.name);
    const textarea = await openModalEditor(page);
    await textarea.press('End');
    await pasteFile(textarea, { base64: PNG_1X1_BASE64, name: 'x.png', type: 'image/png' });
    const markdown = (await waitForImageInBody(request, cardA.id)).match(IMAGE_MARKDOWN)![0];
    await page.locator('[title="Restore to board"]').click();

    // The markdown is moved as text, to a card on another board.
    const boardB = await apiCreateBoard(request, `images-b-${Date.now()}`);
    const colB = await apiCreateColumn(request, boardB.name, 'Column');
    await apiCreateCard(request, colB.id, `Destination\n\n${markdown}`);
    await apiUpdateCard(request, cardA.id, { body: 'Source, image cut out' });
    await apiDeleteCard(request, cardA.id);

    await gotoBoardView(page, boardB.name);
    expect(await loadedWidth(page.locator('.card-preview img').first())).toBe(1);
  });

  test('restoring an earlier version brings a removed image back', async ({ page, request }) => {
    const board = await apiCreateBoard(request, `images-restore-${Date.now()}`);
    const col = await apiCreateColumn(request, board.name, 'Column');
    const card = await apiCreateCard(request, col.id, 'Versioned');
    await gotoBoardView(page, board.name);
    const textarea = await openModalEditor(page);
    await textarea.press('End');
    await pasteFile(textarea, { base64: PNG_1X1_BASE64, name: 'x.png', type: 'image/png' });
    const withImage = await waitForImageInBody(request, card.id);
    await page.locator('[title="Restore to board"]').click();

    // Remove the image, then restore the version that had it.
    await apiUpdateCard(request, card.id, { body: 'Versioned, image removed' });
    const history = await apiBoardHistory(request, board.name);
    const version = history.find(
      entry =>
        entry.entity_id === card.id &&
        (entry.snapshot_after as { body?: string } | undefined)?.body === withImage
    );
    expect(version, 'a history row whose body has the image').toBeDefined();
    await apiRestoreAudit(request, version!.id);

    await page.reload();
    await page.waitForSelector('.columns-row');
    expect(await loadedWidth(page.locator('.card-preview img').first())).toBe(1);
  });
});
