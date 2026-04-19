# Bored Cards Backup — 2026-04-19

Board: **bored-project** (`01kphf6btaf1bx7qnq9t2y24gk`)

---

## TODO (`01kphf6gr4kt1wzabj80mm4pme`)

### Card `01kphf8e1q3kwgd6qrhw69vqy3` (pos 0)
# Auth (OIDC + PKCE)

OIDC auth code + PKCE flow via `openidconnect`. Session middleware guards all `/api/*` routes. Also adds `Authorization: Bearer <token>` path for the MCP server.

## Key additions
- `/auth/login`, `/auth/callback`, `/auth/logout`
- `tower-sessions` backed by SurrealDB (`tower-sessions-sqlx-store`)
- Session cookie: `HttpOnly; Secure; SameSite=Lax`
- Every mutation stamps `last_edited_by = current_user` on boards, columns, and cards
- Schema: add `last_edited_by ON boards|columns|cards TYPE record<users>`

## Env vars
`OIDC_ISSUER_URL`, `OIDC_CLIENT_ID`, `OIDC_CLIENT_SECRET`, `OIDC_REDIRECT_URI`, `SESSION_SECRET`

## Acceptance
- Unauthenticated requests to `/api/*` → 401
- Valid Bearer token accepted on all `/api/*` routes
- Logout destroys session and redirects to Authentik end-session endpoint

**Deliverable:** App is login-gated via Authentik. MCP continues to work via Bearer token.

---

### Card `01kphgzcd3a035gg1q7zd5z6rx` (pos 0)
# UI enhancements
1) When clicking on a card in the board expand it vertically to show the card content as long as it fits on screen, the editor will behave the same unless the card is maximised.
1) add an icon to make the card 'maximisable' as a toggle by a button in the popup title bar, when the card is maximised the route will change and can be shared.
1) make the card editor content scrollable
1) the page footer is too large make it as small as possible and position the watermark in the board popup
1) there is blank space above the column titles remove this
1) cards by default should have a 2 lines more space in the board if they require it
1) in the board when the card content cannot be rendered in full then fade the last line
1) when adding a card by default it goes to the top of the column
1) when creating a new board do not add the todo and done columns
1) New card editor should be the same as the edit card editor

---

### Card `01kphhp28yrgm6crac43kecvz6` (pos 0)
# Automated UI Testing
Add automated ui testing for all features currently existing and for new features
This should be performed using headless browser in playwright
This should also cover sse based edits

---

### Card `01kpk0zfzjxsfzcv2nssycexj5` (pos 1)
# BUG: New card editor title 

is "New card in {column_name.clone()}"

---

### Card `01kphf8y4qkhjfb198hkcck1mn` (pos 3)
# Board Ownership

Boards are private by default. Owners can invite members by email. Members can read/write cards and columns but cannot delete the board or manage invitations.

## Schema changes
- `boards`: add `owner TYPE record<users>`
- New `board_members` table: `board`, `user`, `role` ('owner' | 'member'); unique index on `(board, user)`
- `users` table: populated from OIDC `sub` + `email` on first login

## New endpoints
- `GET  /api/boards/:id/members`
- `POST /api/boards/:id/invites` — `{ email }` (owner only)
- `DELETE /api/boards/:id/members/:user_id` (owner only)
- `PUT /api/boards/:id/transfer` — `{ user_id }`

## Acceptance
- Non-member gets 404 on `GET /api/boards/:id`
- Member cannot delete board (403)
- Removing a member revokes access

**Deliverable:** Boards are private and access-controlled.

---

### Card `01kphf93hejdwydhm0fssmbkk5` (pos 4)
# Soft Delete

All destructive operations become soft deletes (`deleted_at`). Trash UI to browse, restore, or permanently delete items. Auto-purge after 30 days.

## Schema changes
- Add `deleted_at TYPE option<datetime> DEFAULT NONE` to `boards`, `columns`, `cards`
- All `SELECT` queries gain `WHERE deleted_at = NONE`
- Cascade soft-delete: deleting a board soft-deletes columns and cards

## New endpoints
- `GET  /api/trash` — grouped by type, ordered by `deleted_at` desc
- `POST /api/trash/:type/:id/restore`
- `DELETE /api/trash/:type/:id` — permanent delete
- `DELETE /api/trash` — empty trash

## Acceptance
- Deleted card appears in trash; restore returns it to the board
- Delete board → all 6 cards appear in trash
- Items auto-purge after 30 days

**Deliverable:** No accidental permanent data loss. All deletes reversible within 30 days.

---

### Card `01kphf99vg7bg3mqdbdrw3exsv` (pos 5)
# Change History

Every edit to a card's body is recorded as an immutable snapshot. History panel in card modal shows timeline with author and timestamp; any version can be previewed and restored.

## Schema changes
```surql
DEFINE TABLE card_history SCHEMAFULL;
DEFINE FIELD card       ON card_history TYPE record<cards>;
DEFINE FIELD body       ON card_history TYPE string;
DEFINE FIELD edited_by  ON card_history TYPE record<users>;
DEFINE FIELD created_at ON card_history TYPE datetime VALUE time::now();
```

## New endpoints
- `GET  /api/cards/:id/history` — descending by `created_at`
- `POST /api/cards/:id/history/:history_id/restore`

## Frontend
- "History" toggle in card modal footer
- Scrollable list: relative time, author name, body preview
- "Restore this version" button; current version marked "Current"

## Acceptance
- Create + 2 edits → 3 history entries
- Restore → body reverts → 4th entry (tagged as restore)
- `DELETE /api/cards/:id/history/:id` → 405

**Deliverable:** Full audit trail for card content. Any version can be previewed and restored.

---

### Card `01kphf9eqj5qpg4yygaj98gzga` (pos 6)
# Git Links

Link cards to commits, branches, and PRs. GitHub API proxy hides the server-side PAT. Status badges on cards fetched from proxy.

## Schema
`git_links`: `card`, `link_type` ('commit'|'branch'|'pr'), `owner`, `repo`, `ref`

## Endpoints
- `GET  /api/cards/:id/git-links`
- `POST /api/cards/:id/git-links` — `{ link_type, owner, repo, ref }`
- `DELETE /api/git-links/:id`
- `GET /api/github/:owner/:repo/commits/:sha`
- `GET /api/github/:owner/:repo/branches/:branch`
- `GET /api/github/:owner/:repo/pulls/:number`

## New SSE events
`GitLinkAdded { card_id, git_link }`, `GitLinkDeleted { card_id, git_link_id }`

## Acceptance
- Git link appears in `GET /api/cards/:id/git-links`
- Proxy returns commit message and author from GitHub
- Cascade delete when card deleted

**Deliverable:** Cards linked to commits, branches, and PRs with live status from GitHub.

---

### Card `01kpk1pch878c6xvxda63qdkye` (pos 6)
# BUG: Re-ordering with drag and drop

When a card is dragged within its column, the order is not updated properly, hitting refresh shows the cards in a different order.
When the user drags to the same column or a different column, while dragging a ghost ticket will be displayed in the location the ticket would go if the drag ended.

---

### Card `01kphf9ma77rbtv8raw8b4t005` (pos 7)
# CI/CD Integration

Woodpecker webhook integration: pipeline status changes update the linked card's `ci_status`. Colour-coded badge on each card in the board view.

## Key additions
- `ci_status` field on cards (`option<string>`: `pending|running|success|failure|cancelled`)
- `POST /api/webhooks/woodpecker` — verifies HMAC signature, maps pipeline to card via git ref, updates `ci_status`
- Frontend: CI status badge on card items (colour-coded)

## New SSE event
`CardCiStatusChanged { card_id, ci_status }`

## CI step addition
Playwright E2E on main: login → create board → add column → add card → move card → verify SSE update in second browser context

## Acceptance
- Webhook with valid HMAC updates card `ci_status` to "success"
- Webhook with bad HMAC → 401
- `CardCiStatusChanged` emitted on webhook receipt

**Deliverable:** CI pipeline status visible as a badge on cards, updated in real time via Woodpecker webhooks.

---

## IN PROGRESS (`01kphf6gvrmrshjkcq3fsepczj`)

### Card `01kpk199vfv9aqqszcp0c76sd1` (pos 0)
# Card numbers — Iteration 12

**Version:** `1.2`  **Branch:** `feat/card-numbers`

Every card gets a globally-unique sequential integer, auto-assigned on creation, displayed as a zero-padded 3-digit badge (`#001`, `#042`) in the top-right of the card in the board view and in the card modal header. Existing cards get numbers assigned on first deployment (in `created_at` order).

## Schema changes
- New `card_counter` table with single record `card_counter:global { count: int }`
- New `number INT DEFAULT 0` field on `cards` (0 = unassigned, used for migration)

## Backend changes
- `DbCard` + `Card` structs: add `number: i32` / `number: u32`
- `create_card`: atomic `UPDATE card_counter:global SET count += 1` then assign to card
- Startup migration: assign sequential numbers (by `created_at`) to all cards where `number = 0`
- All card responses include `number`

## Frontend changes
- `CardItem`: `#NNN` badge top-right (zero-padded, muted zinc-500 text)
- `card_modal`: number shown in header

## Acceptance
- New cards get incrementing numbers unique across all boards
- Existing cards numbered on first deploy (created_at order)
- Board view shows `#NNN` badge on each card
- Modal shows the number
- MCP `get_card`/`list_cards` include `number`
- Atomic — no duplicates under concurrent creates

---

## DONE (`01kphf6hgm3h23qsyc4xews0jw`)

### Card `01kphf6sy9v909q8s39pe8gpc2` (pos 0)
# Iteration 1 — Walking Skeleton

**Version:** `0.1`

Cargo workspace with backend stub, Dockerfile (backend-only), Woodpecker CI pipeline, and mini-config `bored-stack/`.

## Acceptance
- `GET /health` returns 200
- Multi-stage Docker build succeeds (<200MB)
- Stack deployed to mini; smoke test passes

**Deliverable:** `https://bored.desync.link/health` returns 200 in production.

---

### Card `01kphf6ysea4dmwdt1hqb0h5aq` (pos 1)
# Iteration 2 — Structured Logging → Grafana Loki

**Version:** `0.2`

HTTP access logs and app logs sent to Loki via `tracing-loki`. Console output for local dev; JSON format in production. Optional at runtime — if `LOKI_URL` unset, console-only.

## Key additions
- `backend/src/observability.rs` — `init()` returning `ObservabilityGuard`
- `TraceLayer::new_for_http()` on the router
- Env vars: `LOKI_URL`, `LOG_LEVEL`, `APP_ENV`
- Loki labels: `app="bored"`, `env`, `version`, `level`

## Acceptance
- Every HTTP request visible in Grafana under `{app="bored"}`
- Backend starts normally without `LOKI_URL`
- Production logs are valid JSON

**Deliverable:** All requests visible in Grafana Explore.

---

### Card `01kphf7295z5q14dw8gmxk634h` (pos 2)
# Iteration 3 — Boards & Columns

**Version:** `0.3`

SurrealDB embedded setup + schema (boards, columns). Shared types. REST CRUD for boards and columns. Leptos SPA served as static files. Frontend: board list + board view pages.

## Acceptance
- Full boards/columns CRUD
- 404 on missing IDs
- Cascade delete columns when board deleted
- Frontend: create and view boards and columns without page reload

**Deliverable:** Create and view boards and columns from the UI. Auth not yet enforced.

---

### Card `01kphf75dgme6zdz96gmdsthj7` (pos 3)
# Iteration 4 — Cards

**Version:** `0.4`

SurrealDB `cards` table + schema. Shared types for cards. REST API: cards CRUD + move endpoint. Frontend: column component with card list, add-card button, card modal, card move.

## Acceptance
- Cards CRUD + move between columns
- Cascade delete when column/board deleted
- Position ordering preserved
- `GET /` returns Leptos SPA HTML

**Deliverable:** Full Kanban board usable end-to-end: boards → columns → cards, move between columns.

---

### Card `01kphf7act0r5x5r000njfkas9` (pos 4)
# Iteration 5 — UI Overhaul

**Version:** `0.5`

Hand-rolled CSS (`frontend/style.css`). Dark & moody design system: zinc palette + indigo accent, CSS custom properties.

## Key changes
- Boards list: responsive grid of clickable cards, inline create form
- Board view: horizontal-scrolling kanban columns, fixed-width with scrollable card list
- Card items: hover highlight (indigo-500 border), click opens modal
- Card modal: dark overlay, editable title/description, delete button
- Global: persistent dark navbar with back-navigation

## Acceptance
- Dark theme (zinc-950 bg, zinc-100 text)
- Board view: columns side-by-side with horizontal scroll
- Zero unstyled HTML

**Deliverable:** App looks polished and usable.

---

### Card `01kphf7ghjqr1ncfwwff1qr4zk` (pos 5)
# Iteration 6 — Markdown Cards

**Version:** `0.6`

Replace `title` + `description` with a single `body` field (markdown). Rendered with `pulldown-cmark` (WASM-compatible).

## Key changes
- Schema: drop `title`, rename `description` → `body` (string, not optional)
- API: `{ body }` on create/update; response keys: `[id, column_id, body, position, created_at, updated_at]`
- Card item: `MarkdownPreview` clamped to ~3 lines
- Card modal: rendered mode (default) ↔ edit mode (monospace textarea) toggle on click/blur
- Auto-save: debounced 500ms; status indicator: Saving… / Saved / Save failed
- Flush pending edits on modal close

## Acceptance
- No `title` or `description` in API responses
- Empty body rejected on create (400)
- Auto-save fires once after typing stops

**Deliverable:** Cards show markdown preview; modal edits markdown with live preview.

---

### Card `01kphf7q1f7f0xndwrt4kqfvc9` (pos 6)
# Iteration 7 — CI / Deployments

**Version:** `0.7`

Two deployment targets: **dev** (any branch, manual trigger) and **prod** (main only, manual trigger). Repo owns its own `deploy/docker-compose.yml` — no `bored-stack/` in mini-config.

## Pipeline steps
- `push` → build step (local Docker build, validates Dockerfile)
- `deployment` → validate → compute-version → push image → deploy-dev or deploy-prod
- `prod` only → tag-release (creates `v0.x.y` git tag)

## Key details
- Version computed from `CARGO_VERSION` + tag count
- `DB_VOLUME`: `bored-dev-db` or `bored-prod-db` (external, created if absent)
- All runtime secrets injected as env vars via pipeline SSH

## Acceptance
- Dev deploy from feature branch updates bored-dev.desync.link
- Prod deploy blocked from non-main branches
- DB persists across deploys

**Deliverable:** Dev and prod environments running; devs validate on dev before promoting.

---

### Card `01kphf7vvv4psgzagj36sb95ea` (pos 7)
# Iteration 8 — SPA Deep-Link Routing

**Version:** `0.8`

Fix: reloading any non-`/` route (e.g. `/boards/:id`) returns 404 from `ServeDir`. Solution: configure `ServeDir` with `not_found_service` pointing at `dist/index.html`.

## Change
```rust
// backend/src/main.rs
.fallback_service(
    ServeDir::new(&static_dir)
        .not_found_service(ServeFile::new(format!("{static_dir}/index.html")))
)
```

## Acceptance
- `GET /boards/any-id` → 200 with SPA HTML
- `GET /api/boards` → still returns JSON
- `GET /some/arbitrary/path` → 200 with SPA HTML

**Deliverable:** Reloading or bookmarking any URL works correctly.

---

### Card `01kphf7zznzxy028g0ckm1v9e6` (pos 8)
# Iteration 9 — Bored MCP

**Version:** `0.9`

Standalone MCP server (`mcp/` crate, `rmcp`) exposing the full bored API as MCP tools. Configured via `BORED_API_URL` + `BORED_API_TOKEN`. Ships against the unprotected API (LAN-only); token auth added in iteration 10.

## Tools
`list_boards`, `create_board`, `delete_board`, `list_columns`, `create_column`, `delete_column`, `list_cards`, `get_card`, `create_card`, `update_card`, `move_card`, `delete_card`

## Acceptance
- All 12 tools exercise the correct REST endpoints
- `delete_board` removes all contents
- `move_card` changes the card's column

**Deliverable:** Claude has full read/write access to bored via MCP tools.

---

### Card `01kphfjec45zkv7gb5btj9kfpc` (pos 9)
# fix: SPA routing 404 on browser refresh

Browser refresh on any non-`/` route (e.g. `/boards/:id`) returns 404. The `ServeDir` fallback to `index.html` is either missing or broken.

## Expected
`GET /boards/any-id` → 200 with SPA HTML

## Actual
`GET /boards/any-id` → 404

## Notes
Iteration 8 was supposed to fix this via `ServeDir::not_found_service(ServeFile::new(...index.html))`. Regression or was never applied correctly.

---

### Card `01kphf8sdx5a2byehvx60rytvb` (pos 10)
# Iteration 11 — SSE + Drag-and-Drop

**Version:** `1.1`

Real-time multi-user updates via SSE. HTML5 drag-and-drop for cards (within and across columns) and column reorder.

## Key additions
- `broadcast::Sender<BoardEvent>` in `AppState`; all mutation routes fire events
- `GET /api/events` SSE endpoint with keepalive
- Frontend: `EventSource` subscriber reconciles remote updates into Leptos signals
- Card drag-and-drop: reorder within column and move across columns
- Column drag-and-drop: grip icon on hover; `PUT /api/boards/:id/columns/reorder` accepts `{ order: [col_id, …] }`

## BoardEvent variants
`CardCreated`, `CardUpdated`, `CardDeleted`, `CardMoved`, `ColumnCreated`, `ColumnUpdated`, `ColumnDeleted`, `ColumnsReordered`, `BoardCreated`, `BoardUpdated`, `BoardDeleted`

## Acceptance
- User B receives `CardCreated` SSE event within 1s of user A creating a card
- Drag result persists after reload
- SSE connection stays alive (keepalive comment after 30s idle)

**Deliverable:** Live multi-user board; changes appear instantly for all connected clients.
