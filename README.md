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
├── Cargo.toml          # workspace: [shared, backend, frontend, mcp]
├── backend/            # Axum API server
├── frontend/           # Leptos WASM SPA
├── shared/             # request/response types (serde)
├── mcp/                # MCP server (bored-mcp) exposing the bored API as tools
├── e2e/                # Playwright suite + mock OIDC compose for CI
├── scripts/            # one-off ops scripts (e.g. mcp_smoke_all.py)
├── deploy/
│   └── docker-compose.yml
├── Dockerfile
└── .woodpecker/
    ├── build.yml          # CI: build + e2e on push; manual deployment pipeline
    └── pr-review.yml.disabled  # Claude PR review agent (disabled; see below)
```

## Versioning

Release identity is owned by [`woodpecker-plugin-release-versions`](https://github.com/vcheesbrough/woodpecker-plugin-release-versions), shared with `v-note`. The rules:

- `Cargo.toml` `[workspace.package].version` stores `MAJOR.MINOR.0` (e.g. `1.24.0`). Only `MAJOR.MINOR` is read — the patch digit is a placeholder. Bump `MAJOR.MINOR` manually when starting an iteration.
- The plugin's `compute` mode allocates the next patch as `max(existing MAJOR.MINOR.* tags) + 1`, or **reuses** the tag already on the commit. The result is written to `.release-tag` (plain `MAJOR.MINOR.PATCH`, no `v` prefix).
- **One semver per commit:** the first successful deploy (dev *or* prod) of a commit mints the tag; later deploys of the same commit reuse it.
- **Git tag == docker tag == burned-in version:** `.release-tag` is the single docker tag, is passed as `--build-arg RELEASE_TAG` and compiled into the binary (`shared::app_version`), and is the git tag pushed after a successful deploy.

Legacy `v1.x` tags from the old scheme remain in history on earlier release lines; new tags are plain semver.

## CI

Woodpecker has two pipelines, both defined in [`.woodpecker/build.yml`](.woodpecker/build.yml):

**On every push or manual run**:

1. **compute-version** — `woodpecker-plugin-release-versions` (`compute` mode) writes `.release-tag`.
2. **build** — builds, verifies, and pushes the production image tagged with `.release-tag`. Lint (`cargo fmt --check`, `cargo clippy -D warnings`) and tests (`cargo test --lib`) run *inside* the Dockerfile's `backend-builder` stage, so a green build implies a green check suite.
3. **e2e** — runs `e2e/docker-compose.test.yml` (mock OIDC + the freshly-built image + Playwright). Reports are written to `/srv/dev/playwright-reports/<pipeline>-<branch>-<sha>/`.
4. **apply-authentik-blueprint-auto-dev** — synchronises the development-only Authentik configuration from [`authentik/blueprint-dev.yaml`](authentik/blueprint-dev.yaml) before rollout; the full dev-and-prod blueprint remains deployment-only.
5. **auto-deploy-dev** — deploys the tested image to the development environment.
6. **tag-release-auto-dev-1.31.0** — creates and pushes the git tag matching `.release-tag` after the successful dev deployment.

**On a manual deployment event** (`CI_PIPELINE_DEPLOY_TARGET=dev|prod`):

1. **validate-deployment** — refuse anything other than `dev` or `prod`; refuse `prod` from non-`main` branches.
2. **compute-version** — `woodpecker-plugin-release-versions` (`compute` mode) allocates/reuses the semver and writes `.release-tag`.
3. **apply-authentik-blueprint** — runs [`woodpecker-plugin-authentik-blueprint`](https://github.com/vcheesbrough/woodpecker-plugin-authentik-blueprint) against [`authentik/blueprint.yaml`](authentik/blueprint.yaml) so Authentik OAuth providers stay in sync before the app rolls out.
4. **push** — build with `--build-arg RELEASE_TAG` + OCI labels, verify metadata, and push the single `:$(cat .release-tag)` image to `registry.desync.link`.
5. **deploy-dev / deploy-prod** — run `docker compose -f deploy/docker-compose.yml up -d --pull always --wait --wait-timeout 120` against the host's docker socket, with the OIDC client secret, image tag, host name, and DB volume injected as env. The step succeeds only after the container is healthy. There is no SSH or `scp` step.
6. **tag-release-dev / tag-release-prod** — *after* a successful deploy, `woodpecker-plugin-release-versions` (`push-tag` mode) creates and pushes the annotated git tag matching `.release-tag` (idempotent: no-op if the commit is already tagged). Runs for **both** dev and prod.

The PR pipeline (`.woodpecker/pr-review.yml.disabled`) runs the Claude PR review agent on every pull request. It is **currently disabled** (renamed to `*.disabled`) because the agent image fails to pull; PR review is handled manually for now (see [`AGENTS.md`](AGENTS.md) §7). Re-enable by renaming back to `pr-review.yml`.

### Required secrets

Pipeline YAML uses `from_secret: <name>` like native Woodpecker secrets, but values are **not** stored in Woodpecker itself: they are fetched from **OpenBao** via the Woodpecker secret extension ([`woodpecker-openbao-broker`](https://github.com/vcheesbrough/woodpecker-openbao-broker)). Add or rotate values in OpenBao under the paths your broker maps for this repo; the names below are the keys the pipeline expects after merge. For `authentik_api_token` and `bored_mcp_prod_client_secret`, use the helper in the mini-config repo: [`scripts/patch-bored-woodpecker-openbao-secrets.sh`](https://github.com/vcheesbrough/mini-config/blob/main/scripts/patch-bored-woodpecker-openbao-secrets.sh) (writes to `secret/woodpecker/repos/vcheesbrough/bored` via `bao kv patch`).

| Secret | Used by |
|---|---|
| `zot_ci_user` / `zot_ci_password` | push to `registry.desync.link` |
| `github_token` | `woodpecker-plugin-release-versions` — remote tag listing + `git push` of release tags |
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

The container runs its own rustls listener on port 443 with a self-signed cert; Traefik terminates the public-facing TLS (Let's Encrypt via `certresolver=myresolver`) and forwards HTTPS to the container. Docker probes `GET /health` on the internal listener every 10 seconds, allowing only that loopback probe to accept the self-signed certificate. After a 15-second startup grace period, three consecutive 3-second failures mark the container unhealthy. Logs are shipped to Loki at `monitor-loki:3100`.

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

## Local development

```bash
# Backend (plain HTTP on :3000 when TLS_CERT/TLS_KEY are unset; anonymous auth when OIDC vars are unset)
cargo run -p backend

# Frontend (requires trunk + the wasm32-unknown-unknown target)
cd frontend && trunk serve
```

If both `TLS_CERT` and `TLS_KEY` point at PEM files, the backend instead binds rustls to `:443`. Inside the production image those paths default to `/app/cert.pem` / `/app/key.pem` (a self-signed cert is generated at image build time).

The full CI suite — fmt, clippy, unit tests, build, and Playwright — can be reproduced locally with the exact commands CI uses (see [`.cursor/rules/woodpecker-after-push.mdc`](.cursor/rules/woodpecker-after-push.mdc) for the canonical recipe).

## License

Source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE). This is **not** an OSI-approved open source license — see [LICENSE-TIER.md](LICENSE-TIER.md).

**Commercial use** — including use in a for-profit organisation's production systems, products, or services — requires a separate license. Contact: vincent@desync.link.
