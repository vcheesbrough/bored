import { APIRequestContext, Page, expect } from '@playwright/test';

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

export async function apiDeleteColumn(request: APIRequestContext, columnId: string) {
  const res = await request.delete(`/api/columns/${columnId}`);
  if (!res.ok()) throw new Error(`DELETE /api/columns/${columnId} failed: ${res.status()} ${await res.text()}`);
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

// ── SSE fixtures for "the tab missed events" tests ────────────────────────

/**
 * Point the page's event stream at a board id nothing will ever publish to.
 * The backend filters by that id and validates nothing, so the response is a
 * perfectly ordinary, perfectly silent SSE stream: connected (so the tab still
 * allows mutations) but never told anything about its own board. Stands in for
 * a receiver lagging out of the backend's 128-slot broadcast channel.
 */
export async function silenceSse(page: Page) {
  await page.route('**/api/events*', route => {
    const url = new URL(route.request().url());
    url.searchParams.set('board_id', 'no-such-board');
    return route.continue({ url: url.toString() });
  });
}

/**
 * Drop every SSE message whose JSON `type` is `type` before the app sees it —
 * what a lagged receiver sees when exactly those events fell out of the
 * broadcast channel. Must be called before the page navigates.
 *
 * The app is wrapped at both ways it could subscribe (`onmessage` and
 * `addEventListener('message', …)`), and every drop is counted in
 * `window.__bored_sse_dropped`. Pair it with {@link expectSseDropped}: if the
 * app ever subscribes some third way, the wrapper would silently filter
 * nothing and the test would pass without testing anything — the count is
 * what makes that failure loud.
 */
export async function dropSseEvents(page: Page, type: string) {
  await page.addInitScript((dropType: string) => {
    const Native = window.EventSource;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const w = window as any;
    w.__bored_sse_dropped = 0;
    /** Wrap a message handler so dropped events never reach it. */
    const filtered = (handler: (ev: MessageEvent) => void) => (ev: Event) => {
      const msg = ev as MessageEvent;
      try {
        if (JSON.parse(msg.data)?.type === dropType) {
          w.__bored_sse_dropped += 1;
          return;
        }
      } catch {
        /* not JSON — pass it through untouched */
      }
      handler(msg);
    };
    w.EventSource = function (...args: unknown[]) {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const es = new (Native as any)(...args);
      Object.defineProperty(es, 'onmessage', {
        set(handler: (ev: MessageEvent) => void) {
          Native.prototype.addEventListener.call(es, 'message', filtered(handler));
        },
        configurable: true,
      });
      es.addEventListener = (
        kind: string,
        handler: (ev: MessageEvent) => void,
        options?: unknown
      ) =>
        kind === 'message'
          ? Native.prototype.addEventListener.call(es, kind, filtered(handler), options)
          : Native.prototype.addEventListener.call(es, kind, handler, options);
      return es;
    };
    w.EventSource.prototype = Native.prototype;
  }, type);
}

/** Assert {@link dropSseEvents} actually intercepted something. */
export async function expectSseDropped(page: Page) {
  await expect
    .poll(async () => page.evaluate(() => (window as any).__bored_sse_dropped ?? 0))
    .toBeGreaterThan(0);
}
