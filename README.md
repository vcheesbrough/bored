# bored

A full-stack Rust Kanban board app. Axum backend, Leptos WASM frontend, SurrealDB embedded, OIDC auth, SSE real-time updates.

## Stack

| Crate | Purpose |
|---|---|
| `leptos` (CSR) | Frontend framework |
| `trunk` | WASM build tool |
| `axum` | Backend web framework |
| `surrealdb` (embedded, SpeeDB) | Database — no separate container |
| `openidconnect` | OIDC auth code + PKCE |
| `tower-sessions` | Server-side session store |
| `axum-server` + rustls | TLS termination |

## Workspace layout

```
bored/
├── Cargo.toml          # workspace: [shared, backend, frontend]
├── backend/            # Axum API server
├── frontend/           # Leptos WASM SPA
├── shared/             # request/response types (serde)
├── deploy/
│   └── docker-compose.yml
├── Dockerfile
└── .woodpecker/
    ├── build.yml       # CI: check → build → push → tag → deploy
    └── pr-review.yml   # Claude PR review agent
```

## Versioning

`Cargo.toml` stores `MAJOR.MINOR` (e.g. `0.1`). The patch component is the Woodpecker pipeline number, injected by CI. Bump `MAJOR.MINOR` manually when merging an iteration PR.

| Iteration | Version |
|---|---|
| 1 — Walking Skeleton | `0.1` |
| 2 — Boards & Columns | `0.2` |
| 3 — Cards | `0.3` |
| 4 — Auth | `0.4` |
| 5 — SSE + Drag-drop | `0.5` |
| 6 — Git Links | `0.6` |
| Public release | `1.0` |

## CI

Woodpecker pipeline on every push and manual trigger:

1. **check** — `cargo fmt`, `cargo clippy`, `cargo test`
2. **build-and-push** — `docker buildx build --push` tagged `:sha`, `:VERSION`, `:latest`
3. **tag-release** — `git tag vVERSION` pushed to GitHub
4. **deploy** — `scp deploy/docker-compose.yml` to mini, `run-stack.sh bored-stack up -d --pull always`

PR pipeline runs the Claude PR review agent on every pull request.

### Required secrets

| Secret | Used by |
|---|---|
| `zot_ci_user` / `zot_ci_password` | push to `registry.desync.link` |
| `github_token` | git tag push |
| `mini_ssh_key` | SSH deploy |
| `anthropic_api_key` | PR review agent |
| `pr_reviewer_gh_app_id` | PR review agent |
| `pr_reviewer_gh_app_installation_id` | PR review agent |
| `pr_reviewer_gh_app_private_key_b64` | PR review agent |

## Deployment

The app runs on mini at `https://bored.desync.link`. TLS is terminated by the container itself (rustls); Traefik proxies HTTPS through to port 443.

The compose file lives in this repo at `deploy/docker-compose.yml` and is copied to mini by CI on each deploy.

### Environment variables (via `secrets.env.enc` in mini-config)

```
GITHUB_TOKEN=            # GitHub PAT, repo read scope
OIDC_ISSUER_URL=         # https://authentik.desync.link/application/o/bored/
OIDC_CLIENT_ID=
OIDC_CLIENT_SECRET=
OIDC_REDIRECT_URI=       # https://bored.desync.link/auth/callback
SESSION_SECRET=          # random 64-byte hex
```

## Local development

```bash
# Backend
cargo run -p backend

# Frontend (requires trunk)
cd frontend && trunk serve
```

TLS cert/key paths default to `/app/cert.pem` / `/app/key.pem`. Override with `TLS_CERT` / `TLS_KEY` env vars.
