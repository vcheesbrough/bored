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
| 2 — Structured Logging | `0.2` | `0.2.3` |
| 3 — Boards & Columns | `0.3` | `0.3.7` |
| 4 — Cards | `0.4` | `0.4.15` |
| 5 — UI Overhaul | `0.5` | `0.5.1` |
| 6 — Markdown Cards | `0.6` | `0.6.4` |
| 7 — Auth | `0.7` | `0.7.3` |
| 8 — SSE + Drag-drop | `0.8` | `0.8.11` |
| 9 — Git Links | `0.9` | `0.9.2` |
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
        SSE real-time updates, tower-sessions, tracing+tracing-loki structured logging, shared types crate.

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

### Iteration 2 — Structured Logging → Grafana Loki

HTTP access logs and application logic logs sent to Grafana Loki via `tracing-loki`. Console output for local dev; JSON format in production. Logging is optional at runtime — if `LOKI_URL` is unset the backend starts normally with console-only output.

**Crate additions (`backend/Cargo.toml`):**
```toml
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "json"] }
tower-http = { version = "0.6", features = ["trace"] }
tracing-loki = { version = "0.2", features = ["compat-0-2-1"] }
url = "2"
```

**New file `backend/src/observability.rs`:**
- `pub fn init() -> ObservabilityGuard` — called as the first line in `main()`
- Reads `LOG_LEVEL` (default `"info"`), `APP_ENV` (default `"development"`), `LOKI_URL` (optional)
- Console layer: pretty in dev, `json().flatten_event(true)` in production
- Optional Loki layer with labels `app="bored"`, `env=<APP_ENV>`, `version=<CARGO_PKG_VERSION>` (level label added automatically by `tracing-loki`)
- `ObservabilityGuard` holds the Loki background task `JoinHandle` — must stay alive for process lifetime
- `_loki_task` field (underscore-prefix, not plain `_`) keeps handle alive

**Changes to `backend/src/main.rs`:**
- `mod observability;` + `use tower_http::trace::TraceLayer;`
- `let _obs = observability::init();` as first line in `main()`
- `TraceLayer::new_for_http()` added to `app()` router (responses logged at INFO, request-open at DEBUG)
- Replace any `println!` startup message with `tracing::info!(addr = %addr, "bored backend listening")`

**New env vars (add to `deploy/docker-compose.yml`):**

| Variable | Default | Purpose |
|---|---|---|
| `LOKI_URL` | absent = disabled | Base URL of Loki instance (e.g. `http://loki:3100`) — crate appends `/loki/api/v1/push` |
| `LOG_LEVEL` | `info` | `EnvFilter` directive (e.g. `info,tower_http=debug`) |
| `APP_ENV` | `development` | Loki `env` label; switches fmt layer (pretty vs JSON) |

**Loki labels per log line:** `app="bored"`, `env=<APP_ENV>`, `version=<CARGO_PKG_VERSION>`, `level=<INFO|DEBUG|WARN|ERROR>`

`CARGO_PKG_VERSION` is set by Cargo at compile time via `env!("CARGO_PKG_VERSION")`, so it reflects the `Cargo.toml` version (MAJOR.MINOR) baked into each build.

**Example LogQL queries:**
```logql
{app="bored", env="production"}
{app="bored", env="production", version="0.2"}
{app="bored", level="ERROR"}
{app="bored"} | json | status >= 500
```

**Tests:** existing `cargo test -p backend` still passes — `TraceLayer` is a no-op when no global subscriber is set. No test calls `observability::init()` to avoid `SetGlobalDefaultError` from multiple-init conflicts.

**Gherkin scenarios:**
```gherkin
Feature: HTTP access logging
  Scenario: Every request produces a log line
    Given the backend is running with LOG_LEVEL=info
    When GET /health is called
    Then a log line is emitted at INFO with method, URI, status, and latency

  Scenario: Log level filters debug events
    Given the backend is running with LOG_LEVEL=info
    When GET /health is called
    Then no DEBUG-level request-open span event is emitted to the console

Feature: Loki shipping
  Scenario: Logs reach Loki when LOKI_URL is set
    Given LOKI_URL points to a running Loki instance
    And APP_ENV=production
    When GET /health is called several times
    Then querying Loki with {app="bored", env="production"} returns those log lines

  Scenario: Backend starts normally without LOKI_URL
    Given LOKI_URL is not set
    When the backend starts
    Then it logs to stdout only with no errors

Feature: Log format
  Scenario: Production logs are JSON
    Given APP_ENV=production
    When the backend emits a log line
    Then the output is valid JSON with top-level fields (timestamp, level, message, etc.)
```

**Deliverable:** Every HTTP request visible in Grafana Explore under `{app="bored"}`. Application code can emit structured events via `tracing::info!()` / `warn!()` / `error!()` and they appear in Loki automatically.

---

### Iteration 3 — Boards & Columns (no auth)

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

### Iteration 4 — Cards

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

### Iteration 5 — UI Overhaul

- Hand-rolled CSS (`frontend/style.css`) linked via Trunk — no extra build tools
- Dark & moody design system (zinc palette + indigo accent, CSS custom properties)
- Boards list: responsive grid of clickable board cards, inline create form
- Board view: horizontal-scrolling kanban columns, each fixed-width with scrollable card list
- Card items: hover highlight, click opens modal
- Card modal: dark overlay, editable title/description, delete button
- Global: persistent dark navbar with app name and back-navigation

**Tests:** `cargo build -p frontend --target wasm32-unknown-unknown` must pass; existing backend tests unchanged.

**CI steps:** unchanged — Trunk copies CSS during Docker build.

```gherkin
Feature: Visual design
  Scenario: Dark theme applied
    Given the app is loaded in a browser
    Then the page background is dark (zinc-950) and text is light (zinc-100)

  Scenario: Boards list shows a card grid
    Given boards exist
    When the user navigates to /
    Then boards are displayed in a responsive grid of card tiles

  Scenario: Board view shows horizontal kanban columns
    Given a board with columns exists
    When the user navigates to /boards/:id
    Then columns are displayed side-by-side with a fixed width and horizontal scroll

  Scenario: Card hover highlights border
    Given a card is visible in a column
    When the user hovers over it
    Then the card border changes to the accent colour (indigo-500)

  Scenario: Card modal has dark overlay
    Given a card is visible
    When the user clicks on it
    Then a modal appears centred on a semi-transparent dark backdrop
```

**Deliverable:** App looks polished and usable. Zero unstyled HTML.

---

### Iteration 6 — Markdown Cards

- Replace `title` + `description` on cards with a single `body` field storing markdown
- Board card items render the first few lines of the body as HTML, visually clamped with CSS
- Card modal opens showing rendered markdown; clicking into the body swaps to a raw monospace textarea for editing; blurring swaps back to the rendered view
- No explicit save — edits are debounced and auto-persisted as the user types
- Markdown rendered with `pulldown-cmark` (pure-Rust, WASM-compatible)
- No HTML sanitisation — single-user trusted content; documented assumption to revisit at the multi-user iteration

**Schema changes:**
- `cards` table: drop `title`, rename `description` → `body`, change type `option<string>` → `string` (default empty)
- Pre-1.0 migration: a one-off `UPDATE cards SET body = string::concat(title, "\n\n", description ?? "")` run before the schema change on the running instance, then `REMOVE FIELD title ON cards` and `REMOVE FIELD description ON cards`. Acceptable to reset in dev.

**API changes:**
- `POST /api/columns/:id/cards` → body `{ body: string }`
- `PUT /api/cards/:id` → body `{ body?, position?, column_id? }`
- `Card` response: `{ id, column_id, body, position, created_at, updated_at }`
- `shared::CreateCardRequest` / `UpdateCardRequest` / `Card` updated accordingly

**Frontend changes:**
- Add `pulldown-cmark` to `frontend/Cargo.toml`
- New `frontend/src/components/markdown.rs` — `MarkdownPreview` component: renders markdown to HTML via `pulldown-cmark` and injects via `inner_html`
- `CardItem` renders `<MarkdownPreview body=… />`, wrapped in a container that uses `-webkit-line-clamp: 3` (or `max-height` + fade-out mask) so only a few lines show
- `CardModal`: single body region that toggles between two visual modes driven by a `editing: RwSignal<bool>`:
  - **Rendered mode (default on open):** `<MarkdownPreview>` in the modal body, clickable; receives focus on click and flips `editing = true`. Empty body shows a muted placeholder (e.g. "*Click to edit*").
  - **Edit mode:** `<textarea>` with `font-family: ui-monospace, SFMono-Regular, Menlo, monospace` and preserved whitespace, showing raw markdown. Autofocuses on entry and places the caret at the click position when practical. `on:blur` flips `editing = false` and restores the rendered view. `Escape` also exits to rendered mode without discarding the typed body.
  - Either mode occupies the same box so the modal doesn't jump when toggling.
  - No Save button — a debounced effect (500 ms after the last keystroke) fires `PUT /api/cards/:id` with the current body. A small status indicator in the modal footer shows `Saving…` / `Saved` / `Save failed`. A final flush on modal close ensures pending edits are persisted before the card signal is dropped.
- `AddCardModal`: single textarea (title input removed); submit disabled while body is empty. Unchanged from explicit-submit because the card doesn't exist yet — we need a single `POST` to create it.
- Styling for rendered markdown inside cards and the modal preview — headings slightly smaller than the modal title, code blocks on `--surface-2`, lists/links themed to the zinc palette

**Tests:**
- Integration (backend, `axum_test`): cards CRUD round-trips `body`; response JSON no longer contains `title` or `description`
- Markdown-specific: creating a card with `body = ""` returns 400; long bodies preserved verbatim
- `cargo build -p frontend --target wasm32-unknown-unknown` passes

**CI steps:** unchanged

```gherkin
Feature: Markdown cards
  Scenario: Create a card with a markdown body
    Given a column exists
    When POST /api/columns/:id/cards is called with {"body": "# Fix login\n- investigate SSO"}
    Then the card is created with that body
    And GET /api/cards/:id returns the same body verbatim

  Scenario: Board preview renders the first lines
    Given a card with body "# Deploy\nSteps:\n1. Tag\n2. Push\n3. Smoke test"
    When the user views the card on the board
    Then "Deploy" is rendered as a heading and the first steps are visible, clamped to roughly 3 lines

  Scenario: Modal opens in rendered mode
    Given a card with body "# Deploy\n- tag release"
    When the user clicks the card to open the modal
    Then the body region shows rendered markdown (heading + bulleted list)
    And no textarea is visible

  Scenario: Clicking the rendered body enters edit mode
    Given the card modal is open in rendered mode
    When the user clicks the body region
    Then the rendered view is replaced by a focused textarea showing the raw markdown
    And the textarea is rendered in a monospace font

  Scenario: Blurring the textarea returns to rendered mode
    Given the card modal is in edit mode
    When the user clicks outside the textarea (or presses Escape)
    Then the textarea is replaced by the rendered markdown view
    And any edits remain in the body

  Scenario: Typing in edit mode updates the rendered view on blur
    Given the card modal is in edit mode with body "hello"
    When the user appends " **world**" and blurs
    Then the rendered view shows "hello world" with "world" in bold weight

  Scenario: Card body auto-saves after the user stops typing
    Given the card modal is open on an existing card
    When the user edits the body and stops typing for 500 ms
    Then a PUT /api/cards/:id is sent exactly once with the latest body
    And the status indicator shows "Saved"

  Scenario: Rapid typing is debounced into a single save
    Given the card modal is open on an existing card
    When the user types continuously for 2 seconds without pausing longer than 500 ms
    Then only one PUT /api/cards/:id is sent, after typing stops
    And the body on the server matches the final textarea content

  Scenario: Closing the modal flushes pending edits
    Given the user has just typed into the body textarea
    When they close the modal before the debounce elapses
    Then the pending body is persisted via PUT /api/cards/:id before the modal unmounts

  Scenario: Card modal has no Save button
    Given the card modal is open
    Then no button labelled "Save" is visible
    And the only modal actions are "Delete" and close (×)

  Scenario: Empty body is rejected on create
    Given the user opens the Add Card modal
    When they submit with an empty body
    Then the submit button is disabled and the request is not sent

  Scenario: No title field remains on the Card API
    Given a card exists
    When GET /api/cards/:id is called
    Then the response JSON keys are exactly [id, column_id, body, position, created_at, updated_at]
    And do not contain "title" or "description"
```

**Deliverable:** Cards on the board show a short rendered-markdown preview. The modal edits markdown with a live preview. No card titles anywhere in the app.

---

### Iteration 7 — Auth (OIDC + PKCE)

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

### Iteration 8 — Real-time (SSE) + Drag-and-Drop

- `broadcast::Sender<BoardEvent>` in `AppState`; all mutation routes fire events
- `GET /api/events` SSE endpoint with keepalive
- Frontend: `EventSource` subscriber reconciles remote updates into Leptos signals
- HTML5 drag-and-drop for card reorder within column and across columns
- Column reorder via drag-and-drop on column headers; a drag handle (⠿ grip icon) appears on column header hover; dropping a column between two others recomputes all `position` values and bulk-updates via `PUT /api/boards/:id/columns/reorder`
- `PUT /api/boards/:id/columns/reorder` accepts `{ order: [col_id, …] }` — an ordered list of all column IDs for the board; server assigns `position = index` for each

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

Feature: Column reorder
  Scenario: Reorder columns via drag and drop
    Given a board has columns [A, B, C]
    When the user drags column C to the first position
    Then the board displays [C, A, B] and persists after reload

  Scenario: Reorder columns via API
    Given a board has columns [A, B, C] with known IDs
    When PUT /api/boards/:id/columns/reorder is called with order [C, A, B]
    Then GET /api/boards/:id/columns returns columns in the new order

  Scenario: Other users see column reorder in real time
    Given two users are viewing the same board
    When user A reorders the columns
    Then user B sees the new column order without reloading
```

**Deliverable:** Live multi-user board; changes from any client appear instantly for all others.

---

### Iteration 9 — Git Links

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
