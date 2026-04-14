# Bored — Full-Stack Rust Board App

## Context
Build a web-based project board app ("bored") as a new standalone repository, deployed to the homelab via a new `bored-stack/` in mini-config. Stack: Leptos (WASM frontend) + Axum (backend API) + SQLite (persistence), with GitHub API integration for linking commits/branches/PRs to cards.

**Authentication:** OIDC auth code flow with PKCE, implemented in the Axum backend. No Authentik forward-auth middleware at the Traefik level — the app owns the full auth flow. Shared workspace for now; ACL/per-user scoping deferred to a later iteration.

---

## New Repo: `bored`

### Workspace layout
```
bored/
├── Cargo.toml              # workspace: [frontend, backend, shared]
├── frontend/               # Leptos WASM SPA
│   ├── Cargo.toml
│   ├── Trunk.toml
│   ├── index.html
│   └── src/
│       ├── main.rs
│       ├── api.rs          # fetch wrappers (gloo-net)
│       ├── pages/
│       │   ├── boards_list.rs
│       │   └── board_view.rs
│       └── components/
│           ├── column.rs
│           ├── card.rs
│           ├── card_modal.rs
│           └── git_links.rs
├── backend/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs         # Axum server, static file serving
│       ├── db.rs           # SurrealDB embedded client setup + schema init
│       ├── schema.surql    # SurrealDB schema definitions
│       ├── auth.rs         # OIDC auth code+PKCE: /auth/login, /auth/callback, /auth/logout
│       ├── middleware.rs   # session guard (reject unauthenticated requests to /api/*)
│       ├── events.rs       # BoardEvent enum + broadcast channel; GET /api/events SSE endpoint
│       ├── routes/
│       │   ├── boards.rs
│       │   ├── columns.rs
│       │   ├── cards.rs
│       │   └── github.rs   # proxy GitHub API calls (hide PAT)
│       └── models.rs
├── shared/
│   ├── Cargo.toml
│   └── src/lib.rs          # request/response types (serde, derive)
├── schema.surql            # SurrealDB schema (applied at startup)
├── Dockerfile
├── .woodpecker/
│   └── build.yml
└── .gitignore
```

### Key crate choices
| Crate | Purpose |
|---|---|
| `leptos` (CSR mode) | Frontend framework |
| `trunk` | WASM build tool |
| `gloo-net` | HTTP fetch in WASM |
| `axum` | Backend web framework |
| `surrealdb` (embedded, SpeeDB backend) | Database — no separate container |
| `serde` / `serde_json` | Serialisation |
| `reqwest` | Server-side GitHub API proxy |
| `ulid` | ID generation (SurrealDB-friendly) |
| `tower-http` | Static file serving, CORS |
| `openidconnect` | OIDC auth code + PKCE client |
| `tower-sessions` (memory store) | Server-side session store |

---

## Database Schema (SurrealDB)

Defined in `backend/src/schema.surql`, applied at startup via `db.query(include_str!("schema.surql"))`.

```surql
DEFINE TABLE boards SCHEMAFULL;
DEFINE FIELD name       ON boards TYPE string;
DEFINE FIELD created_at ON boards TYPE datetime VALUE $before OR time::now();
DEFINE FIELD updated_at ON boards TYPE datetime VALUE time::now();

DEFINE TABLE columns SCHEMAFULL;
DEFINE FIELD board      ON columns TYPE record<boards>;
DEFINE FIELD name       ON columns TYPE string;
DEFINE FIELD position   ON columns TYPE int;
DEFINE FIELD created_at ON columns TYPE datetime VALUE $before OR time::now();
DEFINE FIELD updated_at ON columns TYPE datetime VALUE time::now();
DEFINE INDEX columns_board ON columns FIELDS board;

DEFINE TABLE cards SCHEMAFULL;
DEFINE FIELD column     ON cards TYPE record<columns>;
DEFINE FIELD title      ON cards TYPE string;
DEFINE FIELD description ON cards TYPE option<string>;
DEFINE FIELD position   ON cards TYPE int;
DEFINE FIELD created_at ON cards TYPE datetime VALUE $before OR time::now();
DEFINE FIELD updated_at ON cards TYPE datetime VALUE time::now();
DEFINE INDEX cards_column ON cards FIELDS column;

DEFINE TABLE git_links SCHEMAFULL;
DEFINE FIELD card       ON git_links TYPE record<cards>;
DEFINE FIELD link_type  ON git_links TYPE string; -- 'commit' | 'branch' | 'pr'
DEFINE FIELD owner      ON git_links TYPE string;
DEFINE FIELD repo       ON git_links TYPE string;
DEFINE FIELD ref        ON git_links TYPE string;
DEFINE FIELD created_at ON git_links TYPE datetime VALUE $before OR time::now();
DEFINE INDEX git_links_card ON git_links FIELDS card;
```

No migration files — schema is idempotent (`DEFINE ... IF NOT EXISTS` or re-applied safely). SurrealDB data stored at `/data/bored.db` using embedded SpeeDB backend.

---

## REST API (backend/src/routes/)

```
GET    /api/events                  SSE stream — push BoardEvents to all connected clients

GET    /api/boards
POST   /api/boards                  { name }
GET    /api/boards/:id
PUT    /api/boards/:id              { name }
DELETE /api/boards/:id

GET    /api/boards/:id/columns
POST   /api/boards/:id/columns      { name, position }
PUT    /api/columns/:id             { name, position }
DELETE /api/columns/:id
POST   /api/columns/:id/reorder     [{ id, position }]  (bulk reorder)

GET    /api/columns/:id/cards
POST   /api/columns/:id/cards       { title, description }
PUT    /api/cards/:id               { title, description, position, column_id }
DELETE /api/cards/:id
POST   /api/cards/:id/move          { column_id, position }

GET    /api/cards/:id/git-links
POST   /api/cards/:id/git-links     { link_type, owner, repo, ref }
DELETE /api/git-links/:id

# GitHub API proxy (uses GITHUB_TOKEN on server, not exposed to client)
GET    /api/github/:owner/:repo/commits/:sha
GET    /api/github/:owner/:repo/branches/:branch
GET    /api/github/:owner/:repo/pulls/:number
```

---

## Real-time Updates (SSE)

**Backend (`events.rs`):**
```rust
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BoardEvent {
    CardCreated { card: Card },
    CardUpdated { card: Card },
    CardDeleted { card_id: String },
    CardMoved  { card_id: String, column_id: String, position: i32 },
    ColumnCreated { column: Column },
    ColumnUpdated { column: Column },
    ColumnDeleted { column_id: String },
    ColumnsReordered { columns: Vec<Column> },
    BoardCreated { board: Board },
    BoardUpdated { board: Board },
    BoardDeleted { board_id: String },
}
```

`AppState` holds a `broadcast::Sender<BoardEvent>` (channel capacity: 128). Every mutation route calls `state.events.send(event)` after the DB write. `GET /api/events` subscribes via `broadcast::Receiver`, streams as `text/event-stream` using `axum::response::Sse`.

**Frontend:**
- On mount of `BoardView`, open `web_sys::EventSource` to `/api/events`
- Parse incoming JSON, dispatch to the appropriate Leptos signal (`cards`, `columns`)
- Optimistic local update on own mutations; incoming SSE events reconcile all other clients
- On unmount, close the `EventSource`

**Crate addition:** no new crates — `axum::response::Sse` is built-in; `web_sys::EventSource` is in the existing `web_sys` dep.

---

## Frontend Components

- **BoardsList** — grid of board cards, create board button
- **BoardView** — horizontal scroll of columns, add column button
  - Drag-and-drop via HTML5 `ondragstart`/`ondrop` on `web_sys` events
  - Optimistic UI: update local signal immediately, fire PATCH in background
- **Column** — column header (rename inline), card list, add card button
- **CardModal** — title/description textarea, git links section, delete button
- **GitLinks** — list linked commits/branches/PRs with live status badges fetched from `/api/github/...`

Routing: `leptos_router` — `/` → boards list, `/boards/:id` → board view

---

## Dockerfile (multi-stage)

```dockerfile
# Stage 1: Build WASM frontend
FROM rust:1.78 AS frontend-builder
RUN rustup target add wasm32-unknown-unknown
RUN cargo install trunk
WORKDIR /app
COPY . .
RUN cd frontend && trunk build --release

# Stage 2: Build backend
FROM rust:1.78 AS backend-builder
WORKDIR /app
COPY . .
RUN cargo build --release -p backend

# Stage 3: Runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=backend-builder /app/target/release/backend ./backend
COPY --from=frontend-builder /app/frontend/dist ./dist
ENV STATIC_DIR=/app/dist
CMD ["./backend"]
```

Axum serves `/dist` as static files at `/` and mounts the REST API at `/api`.

---

## Woodpecker CI (.woodpecker/build.yml)

```yaml
steps:
  - name: build-and-push
    image: woodpecker/docker-buildx:latest
    secrets: [registry_user, registry_password, woodpecker_token]
    commands:
      - docker buildx build --push
          -t registry.desync.link/bored:${CI_COMMIT_SHA}
          -t registry.desync.link/bored:latest .
  - name: deploy
    image: alpine/ssh
    secrets: [mini_ssh_key]
    commands:
      - ssh vincent@mini "./run-stack.sh kanban-stack up -d --pull always"
```

---

## mini-config: `bored-stack/`

### docker-compose.yml
```yaml
services:
  bored:
    image: registry.desync.link/bored:latest@sha256:<pinned>
    container_name: bored
    restart: unless-stopped
    environment:
      DATABASE_PATH: /data/bored.db
      GITHUB_TOKEN: ${GITHUB_TOKEN:?GITHUB_TOKEN is required}
      OIDC_ISSUER_URL: ${OIDC_ISSUER_URL:?OIDC_ISSUER_URL is required}
      OIDC_CLIENT_ID: ${OIDC_CLIENT_ID:?OIDC_CLIENT_ID is required}
      OIDC_CLIENT_SECRET: ${OIDC_CLIENT_SECRET:?OIDC_CLIENT_SECRET is required}
      OIDC_REDIRECT_URI: ${OIDC_REDIRECT_URI:?OIDC_REDIRECT_URI is required}
      SESSION_SECRET: ${SESSION_SECRET:?SESSION_SECRET is required}
    volumes:
      - bored-data:/data
    networks:
      - proxy-backend
    healthcheck:
      test: ["CMD", "wget", "-qO-", "http://localhost:3000/health"]
      interval: 30s
      timeout: 10s
      retries: 3
    labels:
      traefik.enable: "true"
      traefik.http.routers.bored.rule: "Host(`bored.desync.link`)"
      traefik.http.routers.bored.entrypoints: websecure
      traefik.http.routers.bored.tls: "true"
      traefik.http.routers.bored.middlewares: "lan-vpn-only@docker,crowdsec@docker,security-headers@docker"
      traefik.http.services.bored.loadbalancer.server.port: "3000"
      deunhealth.restart.on.unhealthy: "true"

volumes:
  bored-data:

networks:
  proxy-backend:
    external: true
```

### Authentication detail — OIDC auth code + PKCE

The Axum backend owns the full OIDC flow using the `openidconnect` crate:

1. Unauthenticated request hits any route → redirect to `/auth/login`
2. `/auth/login` — generate PKCE verifier/challenge + state, store in temporary session, redirect to Authentik authorization endpoint
3. Authentik redirects to `/auth/callback?code=...&state=...`
4. `/auth/callback` — verify state, exchange code for tokens using code_verifier, validate ID token, create session, set `HttpOnly; Secure; SameSite=Lax` cookie
5. `/auth/logout` — destroy session, redirect to Authentik end-session endpoint

Session store: `tower-sessions` backed by the same SQLite DB (via `tower-sessions-sqlx-store`).

No `authentik@docker` Traefik middleware — the app enforces auth directly. Traefik still applies `lan-vpn-only`, `crowdsec`, and `security-headers`.

**Required env vars:**
```
OIDC_ISSUER_URL=https://authentik.desync.link/application/o/bored/
OIDC_CLIENT_ID=<from Authentik>
OIDC_CLIENT_SECRET=<from Authentik>
OIDC_REDIRECT_URI=https://bored.desync.link/auth/callback
SESSION_SECRET=<random 64-byte hex>
```

### secrets.env.enc additions
- `GITHUB_TOKEN` — GitHub PAT with `repo` read scope
- `OIDC_CLIENT_ID` / `OIDC_CLIENT_SECRET` — from Authentik application config
- `OIDC_ISSUER_URL` — `https://authentik.desync.link/application/o/bored/`
- `OIDC_REDIRECT_URI` — `https://bored.desync.link/auth/callback`
- `SESSION_SECRET` — random 64-byte hex

### Authentik application setup
Create an OAuth2/OIDC provider in Authentik:
- Grant type: Authorization Code
- Client type: Confidential
- Redirect URI: `https://bored.desync.link/auth/callback`
- Scopes: `openid profile email`

---

## Changes to mini-config

1. Add `bored-stack/docker-compose.yml`
2. Add `GITHUB_TOKEN` to `secrets.env.enc`
3. Add bored checks to `smoke-test.sh`:
   - Container running + healthy
   - HTTP 200 on `https://bored.desync.link`
4. Update `RECOVERY.md`: add bored-data volume to backup section
5. Update `README.md`: add bored-stack to stack list

---

## Future: `bored-mcp` container

Deferred. When built:
- Separate container, LAN/VPN-only (no Traefik public exposure)
- Thin MCP server (Node.js or Rust) translating tool calls to REST API calls
- Backend will need an API key auth path alongside OIDC for service-to-service calls
- A `search_cards` endpoint on the REST API will be needed — worth keeping in mind when designing the SurrealDB queries

---

## Features (Gherkin — sequential)

---

### Feature 1: Repository & Workspace Scaffold
```gherkin
Feature: Project scaffold
  Scenario: Cargo workspace initialised
    Given a new directory "bored"
    When the Cargo workspace is created with members [shared, backend, frontend]
    Then `cargo build` succeeds with no errors

  Scenario: Repository pushed to GitHub
    Given the bored directory is a git repository
    When PLAN.md is committed and pushed to vcheesbrough/bored on main
    Then the repository is visible on GitHub
```

---

### Feature 2: Backend boots and serves health endpoint
```gherkin
Feature: Backend health
  Scenario: Server starts
    Given DATABASE_PATH points to a writable directory
    When the backend binary is started
    Then GET /health returns 200 OK

  Scenario: SurrealDB schema applied at startup
    Given the backend has started
    Then the boards, columns, cards, and git_links tables exist in SurrealDB
```

---

### Feature 3: Authentication (OIDC + PKCE)
```gherkin
Feature: Authentication
  Scenario: Unauthenticated user is redirected to login
    Given a user is not logged in
    When they navigate to https://bored.desync.link
    Then they are redirected to /auth/login
    And then redirected to the Authentik authorization endpoint

  Scenario: Successful OIDC callback creates a session
    Given Authentik has authenticated the user
    When the authorization code is exchanged at /auth/callback
    Then a session cookie is set
    And the user is redirected to /

  Scenario: Authenticated user can access the app
    Given a valid session cookie is present
    When the user navigates to /
    Then they see the board list (HTTP 200)

  Scenario: Unauthenticated API requests are rejected
    Given no session cookie is present
    When GET /api/boards is requested
    Then the response is 401 Unauthorized

  Scenario: Logout destroys the session
    Given a valid session cookie is present
    When the user navigates to /auth/logout
    Then the session is destroyed
    And the user is redirected to the Authentik end-session endpoint
```

---

### Feature 4: Boards CRUD
```gherkin
Feature: Boards
  Scenario: Create a board
    Given the user is authenticated
    When POST /api/boards is called with body {"name": "My Board"}
    Then the response is 201 Created
    And the board appears in GET /api/boards

  Scenario: Rename a board
    Given a board named "My Board" exists
    When PUT /api/boards/:id is called with {"name": "Renamed Board"}
    Then GET /api/boards/:id returns name "Renamed Board"

  Scenario: Delete a board
    Given a board exists
    When DELETE /api/boards/:id is called
    Then GET /api/boards/:id returns 404
    And all columns and cards belonging to it are also deleted
```

---

### Feature 5: Columns CRUD
```gherkin
Feature: Columns
  Scenario: Create a column
    Given a board exists
    When POST /api/boards/:id/columns is called with {"name": "To Do", "position": 0}
    Then the column appears in GET /api/boards/:id/columns

  Scenario: Rename a column
    Given a column exists
    When PUT /api/columns/:id is called with {"name": "In Progress"}
    Then the column name is updated

  Scenario: Reorder columns
    Given a board has columns [A, B, C]
    When POST /api/columns/:id/reorder is called with new positions
    Then GET /api/boards/:id/columns returns columns in the new order

  Scenario: Delete a column
    Given a column with cards exists
    When DELETE /api/columns/:id is called
    Then the column is gone and all its cards are deleted
```

---

### Feature 6: Cards CRUD
```gherkin
Feature: Cards
  Scenario: Create a card
    Given a column exists
    When POST /api/columns/:id/cards is called with {"title": "Fix bug", "description": "..."}
    Then the card appears in GET /api/columns/:id/cards

  Scenario: Edit a card
    Given a card exists
    When PUT /api/cards/:id is called with updated title and description
    Then the card reflects the new values

  Scenario: Move a card to another column
    Given columns A and B exist, and card X is in column A
    When POST /api/cards/:id/move is called with {"column_id": B, "position": 0}
    Then card X appears in column B and is gone from column A

  Scenario: Delete a card
    Given a card exists
    When DELETE /api/cards/:id is called
    Then the card no longer appears in its column
```

---

### Feature 7: Git Links
```gherkin
Feature: Git links
  Scenario: Add a commit link to a card
    Given a card exists
    When POST /api/cards/:id/git-links is called with {"link_type": "commit", "owner": "vcheesbrough", "repo": "bored", "ref": "abc1234"}
    Then the link appears in GET /api/cards/:id/git-links

  Scenario: Fetch commit details via GitHub proxy
    Given a git link with type "commit" exists on a card
    When GET /api/github/vcheesbrough/bored/commits/abc1234 is called
    Then the response contains commit message and author from the GitHub API

  Scenario: Delete a git link
    Given a git link exists
    When DELETE /api/git-links/:id is called
    Then the link no longer appears on the card
```

---

### Feature 8: Real-time Updates (SSE)
```gherkin
Feature: Real-time updates
  Scenario: Client receives event when a card is created by another user
    Given user A and user B are both connected to GET /api/events
    When user A creates a card via POST /api/columns/:id/cards
    Then user B receives a CardCreated SSE event within 1 second

  Scenario: Client receives event when a card is moved
    Given two users are connected
    When one user moves a card via POST /api/cards/:id/move
    Then the other user receives a CardMoved SSE event

  Scenario: SSE connection stays alive
    Given a client connects to GET /api/events
    When 30 seconds pass with no mutations
    Then the connection is still open (keepalive comment sent)
```

---

### Feature 9: Frontend — Board List
```gherkin
Feature: Frontend board list
  Scenario: View all boards
    Given the user is authenticated and boards exist
    When they navigate to /
    Then they see a card for each board

  Scenario: Create a board from the UI
    Given the user is on the board list page
    When they enter a name and submit the create form
    Then the new board appears in the list without a page reload
```

---

### Feature 10: Frontend — Board View with Columns and Cards
```gherkin
Feature: Frontend board view
  Scenario: View columns and cards
    Given a board with columns and cards exists
    When the user navigates to /boards/:id
    Then columns are displayed left-to-right with their cards stacked

  Scenario: Add a column
    Given the user is on a board view
    When they click "Add column" and enter a name
    Then the new column appears immediately

  Scenario: Add a card
    Given a column is visible
    When the user clicks "Add card" and enters a title
    Then the card appears at the bottom of the column

  Scenario: Open card detail modal
    Given a card is visible
    When the user clicks on it
    Then a modal opens showing title, description, and git links
```

---

### Feature 11: Frontend — Drag and Drop
```gherkin
Feature: Drag and drop
  Scenario: Move card within a column
    Given a column has cards [X, Y, Z]
    When the user drags card Z above card X
    Then the order becomes [Z, X, Y] and persists after reload

  Scenario: Move card to another column
    Given columns A (card X) and B (empty)
    When the user drags card X into column B
    Then column A is empty and column B contains card X

  Scenario: Other users see drag result in real time
    Given two users are viewing the same board
    When user A drags a card to a new column
    Then user B sees the card move without reloading
```

---

### Feature 12: Dockerfile & Deployment
```gherkin
Feature: Docker build and deployment
  Scenario: Multi-stage Docker build succeeds
    Given the Dockerfile is present
    When `docker build -t bored .` is run
    Then the image builds without errors
    And the image size is reasonable (< 200MB)

  Scenario: Container serves the app
    Given the bored Docker image is running with required env vars
    When GET /health is called
    Then 200 OK is returned
    And GET / returns the Leptos SPA HTML

  Scenario: Stack deployed to mini
    Given bored-stack/docker-compose.yml exists in mini-config
    When `./run-stack.sh bored-stack up -d` is run on mini
    Then the container starts healthy
    And smoke-test.sh passes for bored checks
```

---

## Implementation Order

1. **Create `bored` repo** — `git init`, initial commit with this plan as `PLAN.md`, push to GitHub as `vcheesbrough/bored`
2. **Cargo workspace scaffold** — `Cargo.toml`, `shared/`, `backend/`, `frontend/` crates with minimal stubs
3. **SurrealDB schema + backend routes** — db setup, schema.surql, REST API
4. **OIDC auth** — `/auth/login`, `/auth/callback`, `/auth/logout`, session middleware
5. **SSE events** — broadcast channel, `/api/events` endpoint, mutation routes fire events
6. **Leptos frontend** — board list, board view, columns, cards, drag-and-drop, card modal, git links
7. **Dockerfile** — multi-stage build
8. **Woodpecker CI** — build + push pipeline
9. **mini-config `bored-stack/`** — compose file, smoke test, RECOVERY.md, README.md

---

## Automated Testing

### Backend (`cargo test`)
- **Unit tests** — per-module, covering handlers, SurrealDB query functions, auth helpers. SurrealDB in-memory mode keeps these fast and stateless.
- **Integration tests** — full Axum app with in-memory SurrealDB via `axum_test`. Covers CRUD routes, auth middleware, and error cases end-to-end.
- **SSE tests** — test client connects to `/api/events`, mutation is performed, correct `BoardEvent` variant asserted within timeout.
- **GitHub proxy tests** — `wiremock` mocks the GitHub API; proxy route tested in isolation.
- **Auth tests** — mock OIDC provider walks the full login → callback flow; session cookie asserted; protected routes return 401 without session.

### Frontend (`wasm-pack test --headless --chrome`)
- Component-level tests for board/column/card rendering.
- Lower priority for initial implementation.

### End-to-end (Playwright)
- Covers golden-path Gherkin scenarios: login → create board → add column → add card → move card → verify real-time update in second browser context.
- Runs in Woodpecker CI after Docker image is started.

### CI Pipeline (Woodpecker)
```
1. cargo fmt --check
2. cargo clippy -- -D warnings
3. cargo test
4. trunk build --release
5. docker build
6. (main branch only) docker push → deploy to mini → playwright E2E
```

---

## Verification

1. `trunk build` in `frontend/` — confirms WASM compiles
2. `cargo test` in `backend/` — unit tests for routes (SurrealDB embedded, in-memory for tests)
3. `docker build` locally — confirms multi-stage build succeeds
4. `./run-stack.sh bored-stack up -d` on mini
5. `./smoke-test.sh` — bored checks pass
6. Navigate to `https://bored.desync.link` in browser:
   - Unauthenticated → redirected to Authentik login → redirected back with session cookie
   - Create a board → add columns → add cards → drag card between columns
   - Open card modal → add a GitHub commit link → badge shows commit status
   - `/auth/logout` destroys session and redirects to Authentik end-session endpoint
