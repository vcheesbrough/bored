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
  body = '',
  tags: string[] = []
) {
  const res = await request.post(`/api/columns/${columnId}/cards`, {
    data: { body, tags },
  });
  if (!res.ok()) throw new Error(`POST /api/columns/${columnId}/cards failed: ${res.status()} ${await res.text()}`);
  return await res.json() as Card;
}

/** Card as returned by the API — mirrors `shared::Card`. */
export interface Card {
  id: string;
  body: string;
  column_id: string;
  number: number;
  tags: string[];
}

export async function apiGetCard(request: APIRequestContext, cardId: string): Promise<Card> {
  const res = await request.get(`/api/cards/${cardId}`);
  if (!res.ok()) throw new Error(`GET /api/cards/${cardId} failed: ${res.status()} ${await res.text()}`);
  return await res.json() as Card;
}

export async function apiDeleteCard(request: APIRequestContext, cardId: string) {
  const res = await request.delete(`/api/cards/${cardId}`);
  if (!res.ok()) throw new Error(`DELETE /api/cards/${cardId} failed: ${res.status()} ${await res.text()}`);
}

export async function apiUpdateCard(
  request: APIRequestContext,
  cardId: string,
  patch: {
    body?: string;
    position?: number;
    column_id?: string;
    tags?: string[];
    audit_edit_session?: string;
  }
) {
  const res = await request.put(`/api/cards/${cardId}`, { data: patch });
  if (!res.ok()) throw new Error(`PUT /api/cards/${cardId} failed: ${res.status()} ${await res.text()}`);
  return await res.json() as Card;
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

/** Row from `GET /api/boards/:slug/history` — mirrors `shared::AuditLogEntry`. */
export interface AuditLogEntry {
  id: string;
  created_at: string;
  actor_sub: string;
  actor_display_name: string;
  entity_type: string;
  entity_id: string;
  board_id: string;
  action: string;
  snapshot_before?: unknown;
  snapshot_after?: unknown;
  restored_from?: string | null;
  batch_group?: string | null;
}

export async function apiBoardHistory(
  request: APIRequestContext,
  boardSlug: string
): Promise<AuditLogEntry[]> {
  const res = await request.get(`/api/boards/${boardSlug}/history`);
  if (!res.ok()) {
    throw new Error(`GET /api/boards/${boardSlug}/history failed: ${res.status()} ${await res.text()}`);
  }
  return (await res.json()) as AuditLogEntry[];
}

export async function apiRestoreAudit(
  request: APIRequestContext,
  auditId: string
): Promise<AuditLogEntry[]> {
  const res = await request.post(`/api/audit/${auditId}/restore`);
  if (!res.ok()) {
    throw new Error(`POST /api/audit/${auditId}/restore failed: ${res.status()} ${await res.text()}`);
  }
  return (await res.json()) as AuditLogEntry[];
}

/** Link as returned by the API — mirrors `shared::CardLink`. */
export interface CardLink {
  id: string;
  predecessor_id: string;
  successor_id: string;
  predecessor_number: number;
  successor_number: number;
  reason: string | null;
}

/**
 * `POST /api/cards/:id/links`. `direction` is the role of `otherCardId`
 * relative to `cardId`: `'successor'` means `cardId → otherCardId`.
 */
export async function apiCreateLink(
  request: APIRequestContext,
  cardId: string,
  direction: 'predecessor' | 'successor',
  otherCardId: string,
  reason?: string
): Promise<CardLink> {
  const res = await request.post(`/api/cards/${cardId}/links`, {
    data: { direction, other_card_id: otherCardId, reason },
  });
  if (!res.ok()) throw new Error(`POST /api/cards/${cardId}/links failed: ${res.status()} ${await res.text()}`);
  return (await res.json()) as CardLink;
}

export async function apiListLinks(
  request: APIRequestContext,
  boardSlug: string
): Promise<CardLink[]> {
  const res = await request.get(`/api/boards/${boardSlug}/links`);
  if (!res.ok()) throw new Error(`GET /api/boards/${boardSlug}/links failed: ${res.status()} ${await res.text()}`);
  return (await res.json()) as CardLink[];
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
