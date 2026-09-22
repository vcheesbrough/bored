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
| `tracing` + `tracing-subscriber` | Structured JSON logs to stdout, collected into Loki |
| `rmcp` | MCP server SDK (used by `mcp/`) |

## Workspace layout

```
bored/
├── Cargo.toml          # workspace: [shared, backend, frontend, mcp] + shared dep/lint tables
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

**Adding a dependency.** If only one member needs it, declare it in that
member's `Cargo.toml`. If a second member needs it, move it to
`[workspace.dependencies]` in the root manifest and have both members write
`dep.workspace = true`; a member that needs extra features adds them on top
(`dep = { workspace = true, features = [...] }`), which is why the workspace
entry carries the features *every* member wants and no more.

**Lints** are set once in `[workspace.lints]` and inherited by every member via
`[lints] workspace = true`. Cargo rejects a member that both inherits and adds
its own entries, so a crate-specific exception goes in source as a `#![allow]`
(see `backend/src/main.rs`).

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
2. **build** — builds and verifies the production image tagged with `.release-tag` **locally** (does *not* push). Lint (`cargo fmt --check`, `cargo clippy -D warnings`) and tests (`cargo test -p backend -p shared`) run *inside* the Dockerfile's `backend-builder` stage, so a green build implies a green check suite.
3. **e2e** — runs `e2e/docker-compose.test.yml` (mock OIDC + the freshly-built local image + Playwright). Reports are written to `/srv/dev/playwright-reports/<pipeline>-<branch>-<sha>/`.
4. **publish-image** — pushes the `.release-tag` image to `registry.desync.link` **only after e2e passes**, so the registry never holds an image from a red e2e run (this is what makes the deployment-path `verify-image` existence check a genuine e2e-tested gate).
5. **apply-authentik-blueprint-auto-dev** — synchronises the development-only Authentik configuration from [`authentik/blueprint-dev.yaml`](authentik/blueprint-dev.yaml) before rollout; the full dev-and-prod blueprint remains deployment-only.
6. **auto-deploy-dev** — deploys the tested image to the development environment.
7. **tag-release-auto-dev** — creates and pushes the git tag matching `.release-tag` after the successful dev deployment.

**On a manual deployment event** (`CI_PIPELINE_DEPLOY_TARGET=dev|prod`):

1. **validate-deployment** — refuse anything other than `dev` or `prod`; refuse `prod` from non-`main` branches.
2. **compute-version** — the same plugin step as on the push pipeline, but here it takes its **reuse** path: the commit was already tagged by the push pipeline's `tag-release-auto-dev`, so `compute` finds that tag and writes it straight back to `.release-tag` (`release tag reused for commit <sha>`). Deployments therefore **never allocate** a new version; they promote the one already built. (Reuse requires plugin ≥ 0.1.1 — 0.1.0 could not see the tag, allocated a phantom next patch, and needed a custom `resolve-release-tag` workaround, since removed.)
3. **apply-authentik-blueprint** — runs [`woodpecker-plugin-authentik-blueprint`](https://github.com/vcheesbrough/woodpecker-plugin-authentik-blueprint) against [`authentik/blueprint.yaml`](authentik/blueprint.yaml) so Authentik OAuth providers stay in sync before the app rolls out.
4. **verify-image** — *promote, don't rebuild.* Pull the `:$(cat .release-tag)` image the push-event pipeline already built + e2e-tested for this commit (the reused tag is byte-identical to what dev is running), and re-assert its version/revision OCI labels. **This is the fail-closed gate:** a commit that never finished the main push pipeline has no tag, so `compute` hands it the next unused patch instead — which was never built or published, so the pull fails and the step says the commit must finish that pipeline first. The revision-label check catches the converse case, a tag whose image belongs to a different commit.
5. **deploy-dev / deploy-prod** — run `docker compose -f deploy/docker-compose.yml up -d --pull always --wait --wait-timeout 120` against the host's docker socket, with the OIDC client secret, image tag, host name, and DB volume injected as env. The step succeeds only after the container is healthy. There is no SSH or `scp` step.
6. **tag-release-dev / tag-release-prod** — *after* a successful deploy, `woodpecker-plugin-release-versions` (`push-tag` mode) creates and pushes the annotated git tag matching `.release-tag` (idempotent from plugin 0.1.1: on an already-tagged commit it logs `already exists at commit` and exits 0). Runs for **both** dev and prod.

The PR pipeline (`.woodpecker/pr-review.yml.disabled`) runs the Claude PR review agent on every pull request. It is **currently disabled** (renamed to `*.disabled`) because the agent image fails to pull; PR review is handled manually for now (see [`AGENTS.md`](AGENTS.md) §7). Re-enable by renaming back to `pr-review.yml`.

### Required secrets

Pipeline YAML uses `from_secret: <name>` like native Woodpecker secrets, but values are **not** stored in Woodpecker itself: they are fetched from **OpenBao** via the Woodpecker secret extension ([`woodpecker-openbao-broker`](https://github.com/vcheesbrough/woodpecker-openbao-broker)). Add or rotate values in OpenBao under the paths your broker maps for this repo; the names below are the keys the pipeline expects after merge. For `authentik_api_token` and `bored_mcp_prod_client_secret`, use the helper in the mini-config repo: [`scripts/patch-bored-woodpecker-openbao-secrets.sh`](https://github.com/vcheesbrough/mini-config/blob/main/scripts/patch-bored-woodpecker-openbao-secrets.sh) (writes to `secret/woodpecker/repos/vcheesbrough/bored` via `bao kv patch`).

| Secret | Used by |
|---|---|
| `zot_ci_user` / `zot_ci_password` | push to `registry.desync.link` |
| `github_token` | `woodpecker-plugin-release-versions` (remote tag listing + `git push` of release tags) **and** the `build` step's `docker build --secret id=github_token` (fetches the private `sovereign-config-provider` git dependency — see [Runtime configuration](#runtime-configuration)) |
| `authentik_api_token` | apply-authentik-blueprint (Authentik admin API) |
| `bored_dev_oidc_client_secret` | blueprint var `AUTHENTIK_BORED_DEV_CLIENT_SECRET` only — deploy-dev reads the OIDC client secret from sovereign-config now, not this secret directly |
| `bored_prod_oidc_client_secret` | blueprint var `AUTHENTIK_BORED_PROD_CLIENT_SECRET` only — deploy-prod reads the OIDC client secret from sovereign-config now, not this secret directly |
| `bored_mcp_prod_client_secret` | blueprint var `AUTHENTIK_BORED_MCP_PROD_CLIENT_SECRET` (MCP OAuth client) |
| `bored_dev_sovereign_access_url` | deploy-dev — read-only sovereign-config connection URL for `/bored/dev/server` |
| `bored_prod_sovereign_access_url` | deploy-prod — read-only sovereign-config connection URL for `/bored/prod/server` |
| `claude_oauth_token` | PR review agent |
| `pr_reviewer_gh_app_id` | PR review agent |
| `pr_reviewer_gh_app_installation_id` | PR review agent |
| `pr_reviewer_gh_app_private_key_b64` | PR review agent |

`bored_dev_session_cookie_key` / `bored_prod_session_cookie_key` are retired — the session cookie key now lives at `session/cookie-key` in sovereign-config alongside the rest of runtime config.

## Deployment

Two environments share the same compose file:

| Env | URL | Container | DB volume | OIDC scope |
|---|---|---|---|---|
| dev | `https://bored-dev.desync.link` | `bored-dev` | `bored-dev-db` | `bored:dev:access` |
| prod | `https://bored.desync.link` | `bored` | `bored-prod-db` | `bored:prod:access` |

The container runs its own rustls listener on port 443 with a self-signed cert; Traefik terminates the public-facing TLS (Let's Encrypt via `certresolver=myresolver`) and forwards HTTPS to the container. Docker probes `GET /health` on the internal listener every 10 seconds, allowing only that loopback probe to accept the self-signed certificate. After a 15-second startup grace period, three consecutive 3-second failures mark the container unhealthy. Logs go to stdout as JSON; the homelab's Alloy collects the container's Docker log stream into Loki, labelling it from the `observability.service.name` / `observability.deployment.environment` labels in `deploy/docker-compose.yml`.

### Environment variables

The full set lives in [`deploy/docker-compose.yml`](deploy/docker-compose.yml):

```
APP_ENV                          # "dev" or "prod" (deploy-script-facing name; forwarded into the
                                  # container as BORED__OBSERVABILITY__ENVIRONMENT, and set on it as
                                  # the observability.deployment.environment label Alloy reads)
APP_BRANCH                       # dev only: the branch this deploy was built from. Reported by
                                  # /api/info for the board watermark; kept out of APP_ENV so it
                                  # does not start a new Loki stream set per push
APP_VERSION                      # optional override; leave unset (see "Version and reload" below)
SOVEREIGN_CONFIG_ACCESS_URL_FILE # sourced from bored_{dev,prod}_sovereign_access_url,
                                  # materialised as a file (not left in the container's process env)
```

Everything else — OIDC settings, the session cookie key, log level, the database
path — is resolved at startup from a layered configuration composition root
(`backend/src/config.rs`), **not** individual compose env vars. See
[Runtime configuration](#runtime-configuration) below.

When `oidc.issuer-url` is unset or blank (local dev / tests) the auth middleware short-circuits and
injects a synthetic `anonymous` claim, so the API stays usable without an IdP.

### Version and reload

`RELEASE_TAG` is compiled into **both** halves of the image — the backend binary and the wasm bundle
(`shared::app_version`). An open browser tab polls `/api/info` and, when the version it gets back is
not the one its own bundle was built from, reloads itself onto the new deploy
([`frontend/src/connection.rs`](frontend/src/connection.rs)). While that poll is failing, or while a
board's SSE stream is down, the navbar shows an `offline` badge and
[`frontend/src/api.rs`](frontend/src/api.rs) refuses every mutation — a stale board must not be
written to.

So **`APP_VERSION` must either be unset or match the image's own `RELEASE_TAG`.** Any other value is
a mismatch the tab can never resolve: it reloads once, comes back still mismatched, and then logs
that it is refusing to reload again rather than looping. The e2e stack sets no override for exactly
this reason.

### Runtime configuration

Bored's runtime config (OIDC, the browser session cookie key, observability settings) is resolved at
startup through three layers, lowest priority first:

1. **in-memory defaults** — ports, log level, database path, service name.
2. **[sovereign-config](https://github.com/vcheesbrough/sovereign-config)** — added only when
   `SOVEREIGN_CONFIG_ACCESS_URL_FILE` (or `SOVEREIGN_CONFIG_ACCESS_URL`) is present and non-blank, so
   local dev, unit tests, and e2e (which have no sovereign-config server) fall back to defaults + env.
3. **`BORED__*` environment overrides**, `__`-nested (e.g. `BORED__OIDC__CLIENT-ID` /
   `BORED__OIDC__CLIENT_ID` → `oidc.client-id` — both kebab- and snake_case leaf spellings work, since
   a POSIX shell variable can't contain `-`).

Each config group is written to its own sub-branch:

```
/bored/{dev,prod}/server/oidc            # issuer-url, client-id, client-secret (secret),
                                          # redirect-uri, required-scope, end-session-url,
                                          # mcp/issuer-url, mcp/client-id
/bored/{dev,prod}/server/session         # cookie-key (secret) — required whenever oidc is configured
/bored/{dev,prod}/server/observability   # environment, log-level
```

`server/*` (`http-port`, `tls-cert`, `tls-key`, `static-dir`, `database-path`) is **never** stored in
sovereign-config — it's image-internal and identical across deployments, set via defaults or the
image's own `BORED__SERVER__*` env (see the `Dockerfile` runtime stage).

`oidc` is bored's one **optional** group — an absent or blank `oidc/issuer-url` runs the server in
auth-disabled mode (a synthetic `anonymous` claim), matching local-dev behavior; a present issuer
makes every other `oidc` leaf (and `session/cookie-key`) a required, fail-closed startup error if
missing.

**Access URLs.** Each environment has one read-only sovereign-config connection scoped to its own
subtree (`/bored/dev/server`, `/bored/prod/server`), stored as a Woodpecker secret **in
sovereign-config itself** — `/woodpecker/repos/vcheesbrough/bored/bored_{dev,prod}_sovereign_access_url`
— alongside bored's other Woodpecker secrets (the mini-config broker cutover). Rotation is
`rotate_connection` + rewrite that Woodpecker secret + redeploy; no app change, no image rebuild.

**Version pin.** `backend/Cargo.toml` pins `sovereign-config-provider` to a sovereign-config
release tag (currently `2.30.2`, matching the deployed server). The provider negotiates a protocol
version on connect, so a server upgrade does not require rebuilding bored; it fails closed only
once the server has retired every protocol version the provider speaks.
`sovereign-config-provider` is a private git dependency; the Docker build fetches it via
`scripts/docker-git-credential.sh`, which needs the `github_token` secret (`--secret
id=github_token,env=GITHUB_TOKEN` on `docker build`). An ordinary `gh` login supplies a token
with enough scope — see [AGENTS.md § Getting `GITHUB_TOKEN`](AGENTS.md#getting-github_token) for
the extraction command and for which local checks remain runnable without one.

## Local development

```bash
# Backend (plain HTTP on :3000 by default; anonymous auth when oidc.issuer-url is unset)
cargo run -p backend

# Frontend (requires trunk + the wasm32-unknown-unknown target)
cd frontend && trunk serve
```

No sovereign-config access URL is set locally, so config comes from defaults + `BORED__*` env only —
see [Runtime configuration](#runtime-configuration). If both `BORED__SERVER__TLS_CERT` and
`BORED__SERVER__TLS_KEY` point at PEM files, the backend instead binds rustls to `:443`. Inside the
production image those paths default to `/app/cert.pem` / `/app/key.pem` (a self-signed cert is
generated at image build time).

The full CI suite — fmt, clippy, unit tests, build, and Playwright — can be reproduced locally with the exact commands CI uses (see [`.cursor/rules/woodpecker-after-push.mdc`](.cursor/rules/woodpecker-after-push.mdc) for the canonical recipe).

## License

Source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE). This is **not** an OSI-approved open source license — see [LICENSE-TIER.md](LICENSE-TIER.md).

**Commercial use** — including use in a for-profit organisation's production systems, products, or services — requires a separate license. Contact: vincent@desync.link.
