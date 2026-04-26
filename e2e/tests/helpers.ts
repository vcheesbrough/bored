import { APIRequestContext, Page } from '@playwright/test';

// ── API helpers (direct HTTP, no browser needed) ──────────────────────────

export async function apiCreateBoard(request: APIRequestContext, name: string) {
  const res = await request.post('/api/boards', { data: { name } });
  if (!res.ok()) throw new Error(`POST /api/boards failed: ${res.status()} ${await res.text()}`);
  return await res.json() as { id: string; name: string };
}

export async function apiCreateColumn(
  request: APIRequestContext,
  boardSlug: string,
  name: string,
  position = 0
) {
  const res = await request.post(`/api/boards/${boardSlug}/columns`, {
    data: { name, position },
  });
  if (!res.ok()) throw new Error(`POST /api/boards/${boardSlug}/columns failed: ${res.status()} ${await res.text()}`);
  return await res.json() as { id: string; name: string; board_id: string };
}

export async function apiCreateCard(
  request: APIRequestContext,
  columnId: string,
  body = ''
) {
  const res = await request.post(`/api/columns/${columnId}/cards`, {
    data: { body },
  });
  if (!res.ok()) throw new Error(`POST /api/columns/${columnId}/cards failed: ${res.status()} ${await res.text()}`);
  return await res.json() as { id: string; body: string; column_id: string; number: number };
}

export async function apiDeleteCard(request: APIRequestContext, cardId: string) {
  const res = await request.delete(`/api/cards/${cardId}`);
  if (!res.ok()) throw new Error(`DELETE /api/cards/${cardId} failed: ${res.status()} ${await res.text()}`);
}

export async function apiMoveCard(
  request: APIRequestContext,
  cardId: string,
  columnId: string,
  position = 0
) {
  const res = await request.post(`/api/cards/${cardId}/move`, {
    data: { column_id: columnId, position },
  });
  if (!res.ok()) throw new Error(`POST /api/cards/${cardId}/move failed: ${res.status()} ${await res.text()}`);
}

// ── Browser helpers ───────────────────────────────────────────────────────

/** Navigate to a board and wait for the columns row to be present. */
export async function gotoBoardView(page: Page, boardSlug: string) {
  await page.goto(`/boards/${boardSlug}`);
  // Wait for the WASM app to load and render the board view.
  await page.waitForSelector('.columns-row');
}

/** Open the board-chooser panel (gear icon in the navbar). */
export async function openChooser(page: Page) {
  await page.locator('.navbar-board-btn').click();
  await page.waitForSelector('.board-chooser', { state: 'visible' });
}

/** Close the board-chooser panel by clicking the backdrop. */
export async function closeChooser(page: Page) {
  await page.locator('.chooser-backdrop').click();
  await page.waitForSelector('.board-chooser', { state: 'hidden' });
}
