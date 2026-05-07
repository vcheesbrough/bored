# bored

A full-stack Rust Kanban board app. Axum backend, Leptos WASM frontend, SurrealDB embedded, OIDC auth, SSE real-time updates.

## Stack

| Crate | Purpose |
|---|---|
| `leptos` (CSR) | Frontend framework |
| `trunk` | WASM build tool |
| `axum` + `axum-server` (rustls) | Backend web framework with TLS |
| `surrealdb` (embedded, `kv-surrealkv`) | Database — no separate container |
| `jsonwebtoken` + `reqwest` | OIDC ID-token / access-token validation against cached JWKS |
| `axum-extra` (cookies) | httpOnly auth cookie handling |
| `tracing` + `tracing-loki` | Structured logs, shipped to Loki in prod |
| `rmcp` | MCP server SDK (used by `mcp/`) |

## Workspace layout

```
bored/
├── Cargo.toml          # workspace: [shared, backend, frontend, mcp, agent]
├── backend/            # Axum API server
├── frontend/           # Leptos WASM SPA
├── shared/             # request/response types (serde)
├── mcp/                # MCP server (bored-mcp) exposing the bored API as tools
├── agent/              # agent-poc: autonomous card annotation agent
├── e2e/                # Playwright suite + mock OIDC compose for CI
├── scripts/            # one-off ops scripts (e.g. mcp_smoke_all.py)
├── deploy/
│   └── docker-compose.yml
├── Dockerfile
└── .woodpecker/
    ├── build.yml          # CI: build + e2e on push; manual deployment pipeline
    └── pr-review.yml      # Claude PR review agent
```

## Versioning

`Cargo.toml` stores `MAJOR.MINOR.0` (e.g. `1.22.0`). CI reads `MAJOR.MINOR` from that file and computes the patch as the count of existing `vMAJOR.MINOR.*` tags on GitHub, so successive prod deploys produce `1.22.0`, `1.22.1`, `1.22.2`, …. Bump `MAJOR.MINOR` manually when merging an iteration PR; the deploy pipeline owns the patch.

Dev images get a `MAJOR.MINOR.PATCH-<sha>` suffix so they are addressable separately from prod releases.

## CI

Woodpecker has two pipelines, both defined in [`.woodpecker/build.yml`](.woodpecker/build.yml):

**On every push** (no deploy):

1. **build** — `docker build` the production image tagged with the commit SHA. Lint (`cargo fmt --check`, `cargo clippy -D warnings`) and tests (`cargo test --lib`) run *inside* the Dockerfile's `backend-builder` stage, so a green build implies a green check suite.
2. **e2e** — runs `e2e/docker-compose.test.yml` (mock OIDC + the freshly-built image + Playwright). Reports are written to `/srv/dev/playwright-reports/<pipeline>-<branch>-<sha>/`.

**On a manual deployment event** (`CI_PIPELINE_DEPLOY_TARGET=dev|prod`):

1. **validate-deployment** — refuse anything other than `dev` or `prod`; refuse `prod` from non-`main` branches.
2. **compute-version** — derive `MAJOR.MINOR` from `Cargo.toml`, count `vMAJOR.MINOR.*` tags on GitHub, write `.version` and `.version-dev`.
3. **apply-authentik-blueprint** — runs [`woodpecker-plugin-authentik-blueprint`](https://github.com/vcheesbrough/woodpecker-plugin-authentik-blueprint) against [`authentik/blueprint.yaml`](authentik/blueprint.yaml) so Authentik OAuth providers stay in sync before the app rolls out.
4. **push** — rebuild and push `:MAJOR.MINOR.PATCH` (and the `-<sha>` dev variant) to `registry.desync.link`.
5. **tag-release** — *(prod only)* `git tag vMAJOR.MINOR.PATCH` pushed to GitHub.
6. **deploy-dev / deploy-prod** — run `docker compose -f deploy/docker-compose.yml up -d --pull always` against the host's docker socket, with the OIDC client secret, image tag, host name, and DB volume injected as env. There is no SSH or `scp` step.

The PR pipeline ([`.woodpecker/pr-review.yml`](.woodpecker/pr-review.yml)) runs the Claude PR review agent on every pull request.

### Required secrets

Pipeline YAML uses `from_secret: <name>` like native Woodpecker secrets, but values are **not** stored in Woodpecker itself: they are fetched from **OpenBao** via the Woodpecker secret extension ([`woodpecker-openbao-broker`](https://github.com/vcheesbrough/woodpecker-openbao-broker)). Add or rotate values in OpenBao under the paths your broker maps for this repo; the names below are the keys the pipeline expects after merge.

| Secret | Used by |
|---|---|
| `zot_ci_user` / `zot_ci_password` | push to `registry.desync.link` |
| `github_token` | tag-count lookup + `git push` of release tags |
| `authentik_api_token` | apply-authentik-blueprint (Authentik admin API) |
| `bored_dev_oidc_client_secret` | deploy-dev + blueprint var `AUTHENTIK_BORED_DEV_CLIENT_SECRET` |
| `bored_prod_oidc_client_secret` | deploy-prod + blueprint var `AUTHENTIK_BORED_PROD_CLIENT_SECRET` |
| `bored_mcp_prod_client_secret` | blueprint var `AUTHENTIK_BORED_MCP_PROD_CLIENT_SECRET` (MCP OAuth client) |
| `claude_oauth_token` | PR review agent |
| `pr_reviewer_gh_app_id` | PR review agent |
| `pr_reviewer_gh_app_installation_id` | PR review agent |
| `pr_reviewer_gh_app_private_key_b64` | PR review agent |

## Deployment

Two environments share the same compose file:

| Env | URL | Container | DB volume | OIDC scope |
|---|---|---|---|---|
| dev | `https://bored-dev.desync.link` | `bored-dev` | `bored-dev-db` | `bored:dev:access` |
| prod | `https://bored.desync.link` | `bored` | `bored-prod-db` | `bored:prod:access` |

The container runs its own rustls listener on port 443 with a self-signed cert; Traefik terminates the public-facing TLS (Let's Encrypt via `certresolver=myresolver`) and forwards HTTPS to the container. Logs are shipped to Loki at `monitor-loki:3100`.

### Environment variables

All values are injected by the deploy pipeline (inline `environment:` map — there is no `.env` file on the host). The full set lives in [`deploy/docker-compose.yml`](deploy/docker-compose.yml); the highlights are:

```
APP_ENV                 # "production" or the dev branch name
APP_VERSION             # MAJOR.MINOR.PATCH (or .PATCH-<sha> for dev)
DATABASE_PATH=/data/bored.db
LOKI_URL=http://monitor-loki:3100
OIDC_ISSUER_URL         # https://auth.desync.link/application/o/bored-{dev,prod}/
OIDC_CLIENT_ID          # bored-browser-{dev,prod}
OIDC_CLIENT_SECRET      # from Woodpecker secret
OIDC_REDIRECT_URI       # https://<host>/auth/callback
OIDC_END_SESSION_URL    # https://auth.desync.link/application/o/bored-{dev,prod}/end-session/
REQUIRED_SCOPE          # bored:{dev,prod}:access
# Prod-only: extra issuer accepted alongside browser tokens, used by the MCP service account.
OIDC_MCP_ISSUER_URL     # https://auth.desync.link/application/o/bored-mcp/
OIDC_MCP_CLIENT_ID      # bored-mcp-prod
```

When `OIDC_ISSUER_URL` is unset (local dev / tests) the auth middleware short-circuits and injects a synthetic `anonymous` claim, so the API stays usable without an IdP.

## agent-poc

`agent-poc` watches the bored SSE stream and automatically appends a transition blockquote to a card's body whenever it is moved between columns. Inference is handled by shelling out to the `claude` CLI, so it draws from your Claude Max subscription quota rather than a separate API key. The binary is built in CI but **not** shipped in the production image (it requires `claude` on `PATH` and is run separately).

### Prerequisites

- The `claude` CLI installed and authenticated (`claude --version` should work).
- A running bored instance (local or remote).

### Environment variables

| Variable | Description |
|---|---|
| `BORED_API_URL` | Base URL of the bored API, e.g. `https://bored.desync.link` (no trailing slash) |
| `BOARD_ID` | ULID of the board to watch |
| `BORED_API_TOKEN` | Optional Bearer token for auth-gated deployments (matches the MCP server convention) |
| `RUST_LOG` | Log level — `info` is a good default |

### Running locally

```bash
BORED_API_URL=https://bored-dev.desync.link \
BOARD_ID=<your-board-id> \
RUST_LOG=info \
cargo run -p agent --bin agent-poc
```

A RustRover run configuration is provided in `.run/Run Agent (dev).run.xml` pre-filled for the dev instance.

### How it works

1. On startup the agent fetches all columns for the board into an in-memory cache so column IDs can be resolved to names quickly.
2. It opens the SSE event stream at `/api/events?board_id=<id>` and processes events forever, reconnecting automatically after any network failure.
3. On each `card_moved` event it calls `claude --print` with the card body and column transition as context. Claude returns the original body with a single blockquote appended.
4. The updated body is written back via `PUT /api/cards/:id`.

## Local development

```bash
# Backend (plain HTTP on :3000 when TLS_CERT/TLS_KEY are unset; anonymous auth when OIDC vars are unset)
cargo run -p backend

# Frontend (requires trunk + the wasm32-unknown-unknown target)
cd frontend && trunk serve
```

If both `TLS_CERT` and `TLS_KEY` point at PEM files, the backend instead binds rustls to `:443`. Inside the production image those paths default to `/app/cert.pem` / `/app/key.pem` (a self-signed cert is generated at image build time).

The full CI suite — fmt, clippy, unit tests, build, and Playwright — can be reproduced locally with the exact commands CI uses (see [`.cursor/rules/woodpecker-after-push.mdc`](.cursor/rules/woodpecker-after-push.mdc) for the canonical recipe).
