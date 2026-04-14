# Bored — Full-Stack Rust Board App

## Context
Build a web-based project board app ("bored") as a new standalone repository, deployed to the homelab via a new `bored-stack/` in mini-config. Stack: Leptos (WASM frontend) + Axum (backend API) + SQLite (persistence), with GitHub API integration for linking commits/branches/PRs to cards.

**Authentication:** OIDC auth code flow with PKCE, implemented in the Axum backend. No Authentik forward-auth middleware at the Traefik level — the app owns the full auth flow. Shared workspace for now; ACL/per-user scoping deferred to a later iteration.

---

## Versioning

Semantic versioning (`MAJOR.MINOR.PATCH`) with `0.x` signalling pre-stability. `1.0` is cut when all iterations are shipped and the API is considered stable.

- `Cargo.toml` stores `MAJOR.MINOR` only (e.g. `0.1`) — bumped manually when merging an iteration PR
- `PATCH` is the Woodpecker build number, injected automatically by CI
- Full version: `0.1.42`, `0.2.7`, etc. — every build is uniquely versioned with no manual patch tracking

| Iteration | `Cargo.toml` version | Example built version |
|---|---|---|
| 1 — Walking Skeleton | `0.1` | `0.1.42` |
| 2 — Boards & Columns | `0.2` | `0.2.7` |
| 3 — Cards | `0.3` | `0.3.15` |
| 4 — Auth | `0.4` | `0.4.3` |
| 5 — SSE + Drag-drop | `0.5` | `0.5.11` |
| 6 — Git Links | `0.6` | `0.6.2` |
| Public release | `1.0` | `1.0.1` |

**On merge to main**, Woodpecker constructs `VERSION=${CARGO_VERSION}.${CI_BUILD_NUMBER}`, tags the git commit (`v0.x.N`), and tags the Docker image with `:<sha>` and `:0.x.N`. No `:latest` tag is applied — deployments must reference an explicit version.

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

A self-signed certificate is generated at image build time and embedded in the image (`/app/cert.pem`, `/app/key.pem`). Override paths via `TLS_CERT` / `TLS_KEY` env vars to mount a real cert in production. The backend binds on 443 using `axum-server` + rustls.

```dockerfile
# Stage 1: Build WASM frontend (added in iteration 2)
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
RUN cargo test -p backend -p shared --lib
RUN cargo build --release -p backend

# Stage 3: Runtime
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y ca-certificates openssl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=backend-builder /app/target/release/backend ./backend
COPY --from=frontend-builder /app/frontend/dist ./dist
RUN openssl req -x509 -newkey rsa:4096 \
        -keyout /app/key.pem \
        -out /app/cert.pem \
        -days 3650 \
        -nodes \
        -subj "/CN=bored"
ENV STATIC_DIR=/app/dist
EXPOSE 443
CMD ["./backend"]
```

Axum serves `/dist` as static files at `/` and mounts the REST API at `/api`. TLS is terminated by `axum-server` (rustls); no separate reverse proxy inside the container.

---

## Woodpecker CI (.woodpecker/build.yml)

```yaml
when:
  branch: main
  event: [push, manual]

steps:
  - name: check
    image: rust:1.78
    commands:
      - cargo fmt -p backend -p shared --check
      - cargo clippy -p backend -p shared -- -D warnings
      - cargo test -p backend -p shared

  - name: build-and-push
    image: woodpecker/docker-buildx:latest
    environment:
      REGISTRY_USER:
        from_secret: registry_user
      REGISTRY_PASSWORD:
        from_secret: registry_password
    commands:
      - |
        echo "${REGISTRY_PASSWORD}" | docker login registry.desync.link -u "${REGISTRY_USER}" --password-stdin
        CARGO_VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/' | sed 's/\.[0-9]*$//')
        VERSION=${CARGO_VERSION}.${CI_BUILD_NUMBER}
        docker buildx build --push \
          -t registry.desync.link/bored:${CI_COMMIT_SHA} \
          -t registry.desync.link/bored:${VERSION} .

  - name: tag-release
    image: alpine/git
    environment:
      GITHUB_TOKEN:
        from_secret: github_token
    commands:
      - |
        CARGO_VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/' | sed 's/\.[0-9]*$//')
        VERSION=${CARGO_VERSION}.${CI_BUILD_NUMBER}
        git tag v${VERSION}
        git push https://x-token:${GITHUB_TOKEN}@github.com/${CI_REPO}.git v${VERSION}

  - name: deploy
    image: alpine/ssh
    environment:
      MINI_SSH_KEY:
        from_secret: mini_ssh_key
    commands:
      - |
        mkdir -p ~/.ssh
        echo "${MINI_SSH_KEY}" > ~/.ssh/id_rsa
        chmod 600 ~/.ssh/id_rsa
        ssh -i ~/.ssh/id_rsa -o StrictHostKeyChecking=no vincent@mini "./run-stack.sh bored-stack up -d --pull always"
```

---

## Claude PR Agent (.woodpecker/pr-review.yml)

Triggers on every pull request. Posts a review comment via the GitHub API using the Woodpecker `GITHUB_TOKEN` secret.

```yaml
when:
  event: pull_request

steps:
  - name: claude-pr-review
    image: ghcr.io/anthropics/claude-code:latest
    secrets: [anthropic_api_key, github_token]
    commands:
      - |
        claude --no-interactive -p "
        You are reviewing a pull request for **bored**, a full-stack Rust Kanban board app.
        Stack: Axum backend, Leptos WASM frontend, SurrealDB embedded, OIDC auth (openidconnect),
        SSE real-time updates, tower-sessions, shared types crate.

        Each PR corresponds to one iteration and must satisfy its Gherkin acceptance scenarios
        before merging. The patch version is injected by CI — the only manual version change
        is bumping MAJOR.MINOR in Cargo.toml when the iteration changes.

        Review the diff for:

        **Correctness**
        - Unwraps or expects that should be proper error handling
        - Fallible SurrealDB queries that discard errors
        - Axum handler return types inconsistent with actual response shapes
        - Shared types in \`shared/\` that diverge from what routes produce or consume

        **Security**
        - Any /api/* route not covered by the session middleware
        - Secrets, tokens, or credentials in code or logs
        - Missing input validation at API boundaries
        - Session fixation or cookie misconfiguration

        **OWASP Top 10** (flag any relevant findings by number)
        - A01 Broken Access Control: routes accessible without valid session, missing authorisation checks, insecure direct object references (e.g. user A can mutate user B's board)
        - A02 Cryptographic Failures: sensitive data (tokens, session secrets) logged or returned in responses, weak or missing TLS configuration, session secrets hardcoded
        - A03 Injection: SurrealDB queries constructed from unsanitised input, any use of raw string interpolation in queries
        - A04 Insecure Design: missing rate limiting on auth endpoints, no PKCE state/nonce validation, CSRF exposure on state-mutating endpoints
        - A05 Security Misconfiguration: debug modes or stack traces exposed in responses, overly permissive CORS, default credentials, unnecessary features enabled
        - A06 Vulnerable and Outdated Components: dependency versions with known CVEs (note if a dep is obviously outdated; full audit is out of scope here)
        - A07 Identification and Authentication Failures: session not invalidated on logout, session cookie missing HttpOnly/Secure/SameSite, tokens stored insecurely client-side
        - A08 Software and Data Integrity Failures: Docker image not pinned by digest in compose, no verification of OIDC token signature or claims
        - A09 Security Logging and Monitoring Failures: authentication failures not logged, no audit trail for board/card mutations
        - A10 Server-Side Request Forgery: GitHub proxy endpoint does not restrict target URLs, user-supplied URLs fetched without validation

        **Tests**
        - Are the Gherkin scenarios for this iteration covered by integration tests?
        - Any happy path with no error case coverage
        - SurrealDB in-memory mode used correctly in tests (not hitting a real DB)

        **Versioning**
        - Cargo.toml MAJOR.MINOR bumped correctly for the iteration (patch stays 0 — CI sets it)

        **General**
        - Unused dependencies added to Cargo.toml
        - Blocking calls inside async handlers (std::thread::sleep, std::fs, etc.)
        - Any behaviour that would break an already-shipped iteration

        Be concise. Flag blockers clearly. Note minor issues separately.
        Do not praise the code or summarise what the PR does — focus on problems.
        " \
        --output review.md

      - |
        gh pr comment ${CI_COMMIT_PULL_REQUEST} \
          --body-file review.md \
          --repo ${CI_REPO}
    environment:
      GH_TOKEN:
        from_secret: github_token
      ANTHROPIC_API_KEY:
        from_secret: anthropic_api_key
```

**Required secrets:** `anthropic_api_key`, `github_token` (PAT with `pull_requests: write`).

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

## Iterations

Each iteration ships to production via Woodpecker CI. The pipeline and deployment infrastructure are established in iteration 1 so every subsequent iteration is a routine push-to-deploy.

---

### Iteration 1 — Walking Skeleton

- Cargo workspace with backend stub (`GET /health` only, no DB)
- Dockerfile (backend-only, no frontend stage yet)
- Woodpecker CI pipeline (fmt check → clippy → test → build → push → deploy)
- mini-config `bored-stack/` docker-compose + smoke test (`GET /health`)

**Tests:**
- Unit: `GET /health` returns 200

**CI steps:** `cargo fmt --check` → `cargo clippy -- -D warnings` → `cargo test` → `docker build` → (main) push + deploy + smoke test

```gherkin
Feature: Project scaffold
  Scenario: Cargo workspace initialised
    Given a new directory "bored"
    When the Cargo workspace is created with members [shared, backend, frontend]
    Then `cargo build` succeeds with no errors

Feature: Backend health
  Scenario: Server starts
    Given DATABASE_PATH points to a writable directory
    When the backend binary is started
    Then GET /health returns 200 OK

Feature: Docker build and deployment
  Scenario: Multi-stage Docker build succeeds
    Given the Dockerfile is present
    When `docker build -t bored .` is run
    Then the image builds without errors
    And the image size is reasonable (< 200MB)

  Scenario: Stack deployed to mini
    Given bored-stack/docker-compose.yml exists in mini-config
    When `./run-stack.sh bored-stack up -d` is run on mini
    Then the container starts healthy
    And smoke-test.sh passes for bored checks
```

**Deliverable:** `https://bored.desync.link/health` returns 200 in production.

---

### Iteration 2 — Boards & Columns (no auth)

- SurrealDB embedded setup + schema (boards, columns tables only)
- `shared/` request/response types for boards + columns
- REST API: boards CRUD + columns CRUD
- Dockerfile gains frontend stage; Leptos SPA served as static files
- Frontend: board list page + board view page (read + create, no drag-drop)

**Tests:**
- Integration (`axum_test`, in-memory SurrealDB): full CRUD for boards and columns, 404 on missing IDs, cascade delete of columns when board is deleted

**CI steps:** same pipeline; `cargo test` now covers board/column routes

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

Feature: Columns
  Scenario: Create a column
    Given a board exists
    When POST /api/boards/:id/columns is called with {"name": "To Do", "position": 0}
    Then the column appears in GET /api/boards/:id/columns

  Scenario: Rename a column
    Given a column exists
    When PUT /api/columns/:id is called with {"name": "In Progress"}
    Then the column name is updated

  Scenario: Delete a column
    Given a column with cards exists
    When DELETE /api/columns/:id is called
    Then the column is gone and all its cards are deleted

Feature: Frontend board list
  Scenario: View all boards
    Given boards exist
    When the user navigates to /
    Then they see a card for each board

  Scenario: Create a board from the UI
    Given the user is on the board list page
    When they enter a name and submit the create form
    Then the new board appears in the list without a page reload

Feature: Frontend board view
  Scenario: View columns
    Given a board with columns exists
    When the user navigates to /boards/:id
    Then columns are displayed left-to-right

  Scenario: Add a column
    Given the user is on a board view
    When they click "Add column" and enter a name
    Then the new column appears immediately
```

**Deliverable:** Create and view boards and columns from the UI. Auth not yet enforced (LAN-only acceptable for this iteration).

---

### Iteration 3 — Cards

- SurrealDB cards table + schema
- `shared/` types for cards
- REST API: cards CRUD + move endpoint
- Frontend: column component with card list, add-card button, card modal (title + description), card move

**Tests:**
- Integration: cards CRUD, move card between columns, cascade delete when column/board deleted, position ordering

**CI steps:** unchanged

```gherkin
Feature: Backend health
  Scenario: SurrealDB schema applied at startup
    Given the backend has started
    Then the boards, columns, cards, and git_links tables exist in SurrealDB

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

Feature: Frontend board view
  Scenario: Add a card
    Given a column is visible
    When the user clicks "Add card" and enters a title
    Then the card appears at the bottom of the column

  Scenario: Open card detail modal
    Given a card is visible
    When the user clicks on it
    Then a modal opens showing title, description, and git links

  Scenario: Container serves the app
    Given the bored Docker image is running with required env vars
    When GET /health is called
    Then 200 OK is returned
    And GET / returns the Leptos SPA HTML
```

**Deliverable:** Full Kanban board usable end-to-end: boards → columns → cards, move cards between columns.

---

### Iteration 4 — Auth (OIDC + PKCE)

- OIDC auth code + PKCE flow (`/auth/login`, `/auth/callback`, `/auth/logout`)
- `tower-sessions` backed by SurrealDB
- Session middleware guarding all `/api/*` routes
- Frontend: unauthenticated requests redirect through login flow

**Tests:**
- Integration: mock OIDC provider walks full login → callback flow, session cookie asserted, protected routes return 401 without session, logout destroys session

**CI steps:** unchanged

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

**Deliverable:** App is login-gated via Authentik. Safe to remove LAN-only restriction.

---

### Iteration 5 — Real-time (SSE) + Drag-and-Drop

- `broadcast::Sender<BoardEvent>` in `AppState`; all mutation routes fire events
- `GET /api/events` SSE endpoint with keepalive
- Frontend: `EventSource` subscriber reconciles remote updates into Leptos signals
- HTML5 drag-and-drop for card reorder within column and across columns
- Column reorder via drag-and-drop + `POST /api/columns/:id/reorder`

**Tests:**
- Integration: test client subscribes to `/api/events`, mutation performed, correct `BoardEvent` variant received within timeout; keepalive comment sent after idle period

**CI steps:** unchanged

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

Feature: Columns
  Scenario: Reorder columns
    Given a board has columns [A, B, C]
    When POST /api/columns/:id/reorder is called with new positions
    Then GET /api/boards/:id/columns returns columns in the new order
```

**Deliverable:** Live multi-user board; changes from any client appear instantly for all others.

---

### Iteration 6 — Git Links

- `git_links` table + schema
- `shared/` types for git links
- REST API: git links CRUD + GitHub API proxy routes
- Frontend: GitLinks section in card modal with status badges fetched from proxy

**Tests:**
- Integration: git links CRUD, cascade delete when card deleted
- GitHub proxy: `wiremock` mocks GitHub API; commit/branch/PR proxy routes tested in isolation

**CI steps:** add Playwright E2E on main (login → create board → add column → add card → move card → verify SSE update in second browser context)

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

**Deliverable:** Cards can be linked to commits, branches, and PRs with live status from GitHub.

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
