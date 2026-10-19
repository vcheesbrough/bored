# bored

A full-stack Rust Kanban board app. Axum backend, Leptos WASM frontend, Postgres, OIDC auth, SSE real-time updates.

## Stack

| Crate | Purpose |
|---|---|
| `leptos` (CSR) | Frontend framework |
| `trunk` | WASM build tool |
| `axum` + `axum-server` (rustls) | Backend web framework with TLS |
| `sqlx` (Postgres) | Database access; schema migrations in `backend/migrations/`, applied at startup |
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

A build's version is computed by [`scripts/compute-release-tag.sh`](scripts/compute-release-tag.sh), and a deployment finds the build to promote with [`scripts/resolve-deploy-tag.sh`](scripts/resolve-deploy-tag.sh); [`woodpecker-plugin-release-versions`](https://github.com/vcheesbrough/woodpecker-plugin-release-versions) (shared with `v-note`) only pushes git tags, after a successful deploy. The rules:

- `Cargo.toml` `[workspace.package].version` stores `MAJOR.MINOR.0` (e.g. `1.24.0`). Only `MAJOR.MINOR` is read — the patch digit is a placeholder. Bump `MAJOR.MINOR` manually when starting an iteration.
- **PATCH is the Woodpecker number of the push pipeline that built the image** (`CI_PIPELINE_NUMBER`), written to `.release-tag` (plain `MAJOR.MINOR.PATCH`, no `v` prefix). Pipeline numbers are unique, so builds running at once on different branches can never collide on a version, and no git state has to be read or reserved first. Patches are sparse (`1.69.339`, `1.69.352`, …): every pipeline consumes a number. Uniqueness rests on Woodpecker's pipeline counter: if the repo is re-created in Woodpecker or its database restored from an old backup, numbers repeat — [`scripts/publish-release-image.sh`](scripts/publish-release-image.sh) then refuses to overwrite an existing version. Recover by starting the next iteration, whose new minor is a fresh version line, rather than bumping the minor mid-iteration (the minor is the iteration number).
- **Git tag == docker tag == burned-in version:** `.release-tag` is the image's docker tag, is passed as `--build-arg RELEASE_TAG` and compiled into the binary (`shared::app_version`), and is the git tag pushed after a successful deploy. Each green build is also pushed as `:commit-<sha>`, which always names the latest green build of that commit.
- **A git tag means "deployed":** it is pushed only after a successful dev or prod deploy, and a commit has at most one. A deployment of a commit that already has one redeploys that same build, even if a re-run push pipeline built the commit again since; a commit never deployed gets its latest green build (`:commit-<sha>`). The build is found through the commit, not `CI_PIPELINE_PARENT`, so restarting a failed deployment or promoting a deployment to another environment works.
- **Tags mark deployed builds on any branch**, not releases: a dev deploy of a feature branch tags that branch's commit, and squash-merges give `main` new commits. So the highest `1.N.*` tag is not necessarily what prod runs — the prod release is the tag on the `main` commit prod was deployed from (`/api/info` reports it). Tags also keep deleted branches' deployed commits reachable. (Before card #461 every push on every branch was tagged, so this is fewer branch tags than before.)

Legacy `v1.x` tags from the old scheme remain in history on earlier release lines; new tags are plain semver.

## CI

Woodpecker has two pipelines, both defined in [`.woodpecker/build.yml`](.woodpecker/build.yml):

**On every push or manual run**:

1. **compute-version** — [`scripts/compute-release-tag.sh`](scripts/compute-release-tag.sh) writes `.release-tag` (see [Versioning](#versioning)).
2. **build** — builds and verifies the production image tagged with `.release-tag` **locally** (does *not* push). Lint (`cargo fmt --check`, `cargo clippy -D warnings`) and tests (`cargo test -p backend -p shared`) run *inside* the Dockerfile's `backend-builder` stage — against a throwaway Postgres that [`scripts/with-test-postgres.sh`](scripts/with-test-postgres.sh) starts beside the build and hands in as the `database_url` build secret — and the `scripts/test-*.sh` script tests inside its `frontend-builder` stage, so a green build implies a green check suite.
3. **e2e** — runs `e2e/docker-compose.test.yml` (mock OIDC, the freshly-built local image behind a Traefik edge as in production, the telemetry ingest and a fake OTLP receiver, and Playwright). Reports are written to `/srv/dev/playwright-reports/<pipeline>-<branch>-<sha>/`. Each pipeline runs it as its own compose project, `bored-e2e-<pipeline number>`, so concurrent pipelines cannot tear down each other's stacks; [`scripts/prune-stale-e2e-projects.sh`](scripts/prune-stale-e2e-projects.sh) first removes projects hours old, which cancelled pipelines leak.
4. **publish-image** — [`scripts/publish-release-image.sh`](scripts/publish-release-image.sh) refuses a version the registry already holds (or cannot rule out), then pushes the `.release-tag` image, and the same image as `:commit-<sha>`, to `registry.desync.link` **only after e2e passes**, so the registry never holds an image from a red e2e run (this is what makes the deployment-path `verify-image` existence check a genuine e2e-tested gate).

A push **never deploys** and pushes no git tag. Dev is deployed by hand, from any branch, with a deployment to `dev` (Woodpecker's Deploy action on a pipeline of the commit). That lets several branches build at once without racing each other for the one shared dev environment. It also means `bored-dev` no longer tracks `main`, and nothing makes a `main` build run on dev before it is deployed to prod: deploy the `main` commit to `dev` first if you want that smoke check against real Authentik, sovereign-config and the Traefik edge, which e2e does not give.

**On a manual deployment event** (`CI_PIPELINE_DEPLOY_TARGET=dev|prod`):

1. **validate-deployment** — refuse anything other than `dev` or `prod`; refuse `prod` from non-`main` branches.
2. **find-commit-tag** — [`scripts/find-commit-tag.sh`](scripts/find-commit-tag.sh) lists the repo's git tags and writes the semver tag already on the commit, if any, to `.release-tag` with `.release-tag-reused=true` (else `false`). It logs only what it found; the plugin's `compute` mode, used for this before, also logged an invented `max(tag)+1` "allocation" no build ever has.
3. **apply-authentik-blueprint-dev / -prod** — runs [`woodpecker-plugin-authentik-blueprint`](https://github.com/vcheesbrough/woodpecker-plugin-authentik-blueprint) so Authentik OAuth providers stay in sync before the app rolls out. A dev deploy, which may come from any branch, applies only [`authentik/blueprint-dev.yaml`](authentik/blueprint-dev.yaml) with only the dev client secret, so a dev deploy of an unmerged branch does not, by itself, touch prod Authentik state; the full dev-and-prod [`authentik/blueprint.yaml`](authentik/blueprint.yaml) is applied only by a prod deploy, which is limited to `main`. This guards against accidents, not a hostile branch: the branch controls its own pipeline YAML and receives the Authentik admin `authentik_api_token`, so a real boundary would need the secret broker to withhold that token and the prod secrets from non-`main` pipelines.
4. **verify-image** — *promote, don't rebuild.* [`scripts/resolve-deploy-tag.sh`](scripts/resolve-deploy-tag.sh) settles `.release-tag`: the commit's existing git tag if it was deployed before, else the version label of `:commit-<sha>`. Then pull that image and re-assert its version/revision OCI labels. **This is the fail-closed gate:** a commit with no green push pipeline has no `:commit-<sha>` image, so the step refuses and says to let its push pipeline go green first. The revision-label check catches an image built from a different commit.
5. **deploy-dev / deploy-prod** — render the telemetry and compose layers from sovereign-config, run [`scripts/migrate-from-surreal.sh`](scripts/migrate-from-surreal.sh) (starts Postgres; imports the old SurrealDB store only while Postgres is still empty — see [Database](#database)), then `docker compose -f deploy/docker-compose.yml up -d --pull always --wait --wait-timeout 120` against the host's docker socket. The step succeeds only after the containers are healthy. There is no SSH or `scp` step.
6. **tag-release-dev / tag-release-prod** — *after* a successful deploy, `woodpecker-plugin-release-versions` (`push-tag` mode) creates and pushes the annotated git tag matching `.release-tag` (idempotent from plugin 0.1.1: on an already-tagged commit it logs `already exists at commit` and exits 0, so a redeploy or dev-then-prod of the same build tags once).

The PR pipeline (`.woodpecker/pr-review.yml.disabled`) runs the Claude PR review agent on every pull request. It is **currently disabled** (renamed to `*.disabled`) because the agent image fails to pull; PR review is handled manually for now (see [`AGENTS.md`](AGENTS.md) §7). Re-enable by renaming back to `pr-review.yml`.

### Required secrets

Pipeline YAML uses `from_secret: <name>` like native Woodpecker secrets, but values are **not** stored in Woodpecker itself: they are fetched from **OpenBao** via the Woodpecker secret extension ([`woodpecker-openbao-broker`](https://github.com/vcheesbrough/woodpecker-openbao-broker)). Add or rotate values in OpenBao under the paths your broker maps for this repo; the names below are the keys the pipeline expects after merge. For `authentik_api_token` and `bored_mcp_prod_client_secret`, use the helper in the mini-config repo: [`scripts/patch-bored-woodpecker-openbao-secrets.sh`](https://github.com/vcheesbrough/mini-config/blob/main/scripts/patch-bored-woodpecker-openbao-secrets.sh) (writes to `secret/woodpecker/repos/vcheesbrough/bored` via `bao kv patch`).

| Secret | Used by |
|---|---|
| `zot_ci_user` / `zot_ci_password` | push to `registry.desync.link` |
| `github_token` | `woodpecker-plugin-release-versions` (`git push` of release tags after a deploy), the `find-commit-tag` step (`git ls-remote --tags`, see [`scripts/find-commit-tag.sh`](scripts/find-commit-tag.sh)), **and** the `build` step's `docker build --secret id=github_token` (fetches the private `sovereign-config-provider` git dependency — see [Runtime configuration](#runtime-configuration)) |
| `authentik_api_token` | apply-authentik-blueprint-dev / -prod (Authentik admin API) |
| `bored_dev_oidc_client_secret` | blueprint var `AUTHENTIK_BORED_DEV_CLIENT_SECRET` only — deploy-dev reads the OIDC client secret from sovereign-config now, not this secret directly |
| `bored_prod_oidc_client_secret` | blueprint var `AUTHENTIK_BORED_PROD_CLIENT_SECRET` only (prod deploys) — deploy-prod reads the OIDC client secret from sovereign-config now, not this secret directly |
| `bored_mcp_prod_client_secret` | blueprint var `AUTHENTIK_BORED_MCP_PROD_CLIENT_SECRET` (MCP OAuth client; prod deploys only) |
| `bored_dev_sovereign_access_url` | deploy-dev — read-only sovereign-config connection URL for `/bored/dev/server` |
| `bored_prod_sovereign_access_url` | deploy-prod — read-only sovereign-config connection URL for `/bored/prod/server` |
| `bored_devops_sovereign_access_url` | deploy-dev / deploy-prod — `SOVEREIGN_CONFIG_URL` for `sovereign-config render`: read-only connection **`bored-devops`** rooted at `/bored/devops` (the telemetry layer; see [Telemetry](#telemetry)) |
| `claude_oauth_token` | PR review agent |
| `pr_reviewer_gh_app_id` | PR review agent |
| `pr_reviewer_gh_app_installation_id` | PR review agent |
| `pr_reviewer_gh_app_private_key_b64` | PR review agent |

`bored_dev_session_cookie_key` / `bored_prod_session_cookie_key` are retired — the session cookie key now lives at `session/cookie-key` in sovereign-config alongside the rest of runtime config.

## Deployment

Two environments share the same compose file:

| Env | URL | Container | Postgres volume | Retired SurrealKV volume | OIDC scope |
|---|---|---|---|---|---|
| dev | `https://bored-dev.desync.link` | `bored-dev` | `bored-dev-pg` | `bored-dev-db` | `bored:dev:access` |
| prod | `https://bored.desync.link` | `bored` | `bored-prod-pg` | `bored-prod-db` | `bored:prod:access` |

The container runs its own rustls listener on port 443 with a self-signed cert; Traefik terminates the public-facing TLS (Let's Encrypt via `certresolver=myresolver`) and forwards HTTPS to the container. Docker probes `GET /health` on the internal listener every 10 seconds, allowing only that loopback probe to accept the self-signed certificate. After a 15-second startup grace period, three consecutive 3-second failures mark the container unhealthy. The health check never depends on telemetry. Traces, logs and metrics leave the process over OTLP (see [Telemetry](#telemetry)); logs are *also* written to stdout as JSON, which the homelab's Alloy collects from the container's Docker log stream into Loki, labelling it from the `observability.service.name` / `observability.deployment.environment` labels in `deploy/docker-compose.yml`. On `docker stop` the backend ends every SSE stream (browsers reconnect to the next container), stops accepting, drains open connections for up to 4 s, flushes telemetry and exits (`stop_grace_period: 15s`).

**Memory.** The backend container is capped at 256 MB with no swap (`mem_limit` / `memswap_limit`). It was 768 MB while the embedded SurrealDB held its whole key index in memory (~530 MB peak, card #470); Postgres (card #471) took that out of the process. The backend reports its own resident memory as `process.memory.usage` — watch it before tightening the cap further.

### Database

Each environment runs its own Postgres 16 (`bored-db` in `deploy/docker-compose.yml`, pinned by digest) on a network private to the compose project; only the backend can reach it. The backend connects with the `database` config group (below) and applies [`backend/migrations/`](backend/migrations/) at startup with `sqlx::migrate!` — a failed migration or an unreachable database stops startup before the listener binds. `POSTGRES_PASSWORD` for the container is rendered from `/bored/devops/<env>/compose/POSTGRES_PASSWORD`, an **alias** of the backend's own `/bored/<env>/server/database/password`, so the two cannot disagree.

**Schema decisions.** Ids are the API's lower-case ULIDs as `text`; timestamps are `timestamptz` (microseconds — exactly the API's wire format); `updated_at` is restamped by a trigger; card numbers come from `card_number_seq`, unique under concurrency and gap-tolerant. Foreign keys are deliberate: `columns → boards` and `cards → columns` are `ON DELETE RESTRICT` (the routes delete children themselves, audited, so a forgotten path fails instead of silently losing history), `card_links → cards` is `ON DELETE CASCADE` (a backstop; the routes delete and audit links first). `audit_log` has no foreign keys — history outlives what it describes. A restore whose parent is gone is a 422 saying which to restore first.

**Backups.** The homelab's nightly backup (mini-config `backup-stack/pre-backup.sh`) takes `pg_dump -Fc` of each environment's database. To take one by hand:

```bash
docker exec bored-bored-db-1 pg_dump -U bored -Fc bored > bored-prod-$(date -u +%Y%m%d-%H%M%SZ).dump   # prod (project `bored`)
docker exec bored-dev-bored-db-1 pg_dump -U bored -Fc bored > bored-dev-$(date -u +%Y%m%d-%H%M%SZ).dump
# restore into an empty database: pg_restore -U bored -d bored --no-owner < file.dump
```

**Rolling back.** Until card #471's follow-up removes it, the old SurrealKV volume is untouched by the move, so redeploying the previous image brings the environment back on SurrealDB — but anything written to Postgres since the move is not in that store and would be lost. Prefer fixing forward; roll back only before any real writes. After a rollback, the next deploy **refuses to run** (`migrate-from-surreal.sh`: Postgres has data but the deployed backend is a SurrealDB build), because carrying on would silently drop whatever was written to SurrealDB meanwhile. To go forward again, decide which store is the truth: to keep SurrealDB's, empty the Postgres volume (`docker compose -p <project> down bored-db && docker volume rm bored-<env>-pg && docker volume create bored-<env>-pg`) so the deploy re-imports; to keep Postgres's, stop the old backend container first.

**The SurrealDB import (temporary).** Until card #471's follow-up removes it, each deploy runs [`scripts/migrate-from-surreal.sh`](scripts/migrate-from-surreal.sh): it starts Postgres and asks `bored-backend migrate-from-surreal --check-empty` whether it holds data. If it does, nothing else happens. If not, it stops the old backend and runs the import from the old SurrealKV volume (mounted read-only; the import copies it aside first), which writes every record in one transaction, rewrites pre-#472 `d'…'` timestamps in audit snapshots, reads everything back and commits only if per-table counts and a SHA-256 over every row match. Rows SurrealDB let dangle (a card whose column is gone, a link to a deleted card) cannot satisfy the foreign keys; they are skipped and listed by id in the `surreal migration committed` log line, with the counts, checksum and rewrite count.

### SQL console

A read-only [pgweb](https://github.com/sosedoff/pgweb) at **`https://<host>/dbconsole`** (card #473, a port of v-note #299), for looking at bored's database while diagnosing something. It is the `sqltool` service in `deploy/docker-compose.yml`, behind the compose profile `sqltool`.

**Who can reach it.** Three gates, in order:

1. **LAN/VPN only** (`lan-vpn-only@docker`), with a rate limit.
2. **Authentik** forward auth through the proxy provider `bored-sql-console-<env>`. Only members of the group **`bored-<env>-sql-console`** get past it; add or remove people in Authentik.
3. **pgweb's own basic auth** as a backstop, which keeps the console closed if Authentik is ever not in front of it. Traefik injects that credential after Authentik approves, so nobody is prompted twice.
   - Its limits: the credential sits in the container's Traefik label. Anyone who can `docker inspect` on mini, or read Traefik's API or dashboard, can read it, but they already control the host.
   - Other containers on `proxy-backend` can reach pgweb directly, skipping the LAN and Authentik gates. For them the backstop is the only gate, and its random 64-hex-character password is what holds.
   - Behind every gate, the console can still only read (the grants below).

The container's healthcheck goes red if an unauthenticated request is ever *not* refused.

**What it can do.** It connects as **`bored_pgweb`**, never as `bored`. That role is created by a migration (`backend/migrations/20261020000000_pgweb_readonly_role.sql`) with:
- SELECT only, including on tables later migrations add. This is the boundary: even a session that switches itself out of read-only mode cannot write.
- read-only transactions and a 30 s `statement_timeout` as the session's *defaults*. A session can change both, so the hard cap on a query is pgweb's own `--query-timeout=30`.
- every statement logged by Postgres, as the audit trail.

pgweb's own `--readonly` mode is a client-side filter: a superuser behind it can still read host files through `pg_read_file`. The role is the real boundary. The session is locked to that connection, so it cannot be repointed from the browser.

**How it is deployed.** The deploy steps set `COMPOSE_PROFILES=sqltool`.
- `scripts/deploy-app.sh` gives the role its password from `/bored/devops/<env>/compose/PGWEB_DB_PASSWORD` once the backend is healthy, and then starts the console.
- The basic-auth pair is `PGWEB_AUTH_USER` / `PGWEB_AUTH_PASS` in the same layer.
- `scripts/attach-sqltool-outpost.sh` attaches the provider to the shared embedded outpost. It only ever appends; the outpost must never be declared in a blueprint.
- `scripts/smoke-sql-console.sh` checks after every deploy that an anonymous request is redirected to Authentik, not served.

**Turning it off.** Remove `sqltool` from `COMPOSE_PROFILES` in `.woodpecker/build.yml`; the deploy and smoke steps share one anchor per environment. The next deploy then removes the container (`rm -sf sqltool`), sets `bored_pgweb NOLOGIN`, and the smoke check skips itself. Turning it back on restores the login. To remove the console completely, also delete the `bored-sql-console-<env>` provider, application and group from the blueprints and from Authentik.

**Metrics.** pgweb's metrics (`:9191/metrics`) are discovered by Alloy through the `observability.metrics.*` labels, as `service_name=bored-sql-console`.

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
OTEL_*                           # handed to the container via deploy/otel.env (env_file), never
                                  # in docker's own env; rendered from /bored/devops/<env>/otel (see "Telemetry")
```

Everything else — OIDC settings, the session cookie key, log level, the database
password — is resolved at startup from a layered configuration composition root
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

1. **in-memory defaults** — ports, log level, the database's port/name/user, service name.
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
/bored/{dev,prod}/server/database        # password (secret); host comes from compose
                                          # (BORED__DATABASE__HOST), port/name/user default to
                                          # 5432 / bored / bored
```

`server/*` (`http-port`, `tls-cert`, `tls-key`, `static-dir`) is **never** stored in
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

### Telemetry

The backend exports **traces, logs and metrics over OTLP** (`http/protobuf`) to the homelab's
collector, `monitor-alloy:4318`, following the machine-global `observability` contract
([`backend/src/observability.rs`](backend/src/observability.rs) is the only place the SDK
appears). It is configured by the standard `OTEL_*` variables and **nothing else** — there are no
telemetry keys in the `observability` config group.

**Where the variables come from.** Each environment has a layer of its own in sovereign-config,
holding the `OTEL_*` leaves as direct children and nothing else:

```
/bored/devops/{dev,prod}/otel/OTEL_SERVICE_NAME            # bored (prod aliases dev)
/bored/devops/{dev,prod}/otel/OTEL_RESOURCE_ATTRIBUTES     # deployment.environment.name=<env>,telemetry_source=otlp
/bored/devops/{dev,prod}/otel/OTEL_EXPORTER_OTLP_ENDPOINT  # alias of /observability/otlp-endpoint
/bored/devops/{dev,prod}/otel/OTEL_EXPORTER_OTLP_PROTOCOL  # http/protobuf (prod aliases dev)
```

The deploy steps render it —
`sovereign-config render /bored/devops/<env>/otel -- ./scripts/compose-up-with-otel.sh … up` —
and the script writes the variables to `deploy/otel.env`, which `deploy/docker-compose.yml`
reads through `env_file:` (`required: false`), unsets them so docker itself never sees them
(the docker CLI is OpenTelemetry-instrumented and would rewrite `OTEL_RESOURCE_ATTRIBUTES`),
and deletes the file once compose has read it. The compose file holds no value. Retargeting or
silencing telemetry is a write to the store and a redeploy, not a commit. The collector address is
one shared leaf, `/observability/otlp-endpoint`, aliased into every environment. `telemetry_source=otlp`
is how Alloy marks bored's OTLP logs `log_source="otlp"` in Loki.

**Off.** No `OTEL_*` variable set means telemetry is off and the process runs exactly as it
otherwise would: stdout JSON, no exporters, no background threads, and one startup line saying
`telemetry off`. That is how `cargo run`, `cargo test` and e2e run. An environment runs without
telemetry by leaving the layer off the render line; `OTEL_SDK_DISABLED=true` in the layer silences
it while keeping the layer, and `OTEL_{TRACES,METRICS,LOGS}_EXPORTER=none` silences one signal.
Inbound `traceparent` is still adopted and propagated when off.

**Fails loudly, never quietly.** With any `OTEL_*` variable present the whole set is validated at
startup: a missing endpoint (never the SDK's `localhost` default), a protocol other than
`http/protobuf`, an unknown sampler or exporter, a sampler ratio outside `[0, 1]`, a propagator
other than `tracecontext`, or a missing `service.name` / `deployment.environment.name` stops the
process. An *unreachable* collector does not: batches are dropped and the SDK says so on stdout.

**The one duplicated fact.** `observability.environment` (the `/api/info` watermark, needed with
telemetry off) and `deployment.environment.name` (the telemetry identity) both state the
environment. The pipeline's `APP_ENV` and the layer's `OTEL_RESOURCE_ATTRIBUTES` must agree; the
backend refuses to start if they do not.

**What is emitted.** A `server` span per request, parented on Traefik's edge span, with a child
span per call that leaves the process; log records carrying `trace_id`/`span_id` (on the OTLP
record and in the stdout JSON); `http.server.request.duration`, `bored.sse.*`, `bored.db.errors`,
`bored.auth.outcomes` and `bored.build.info` (version and revision, compiled in). See AGENTS.md
§ Observability for what is still short of the contract.

### Client telemetry

The SPA sends its **own traces and logs** (`service.name=bored-spa`, OTLP/HTTP JSON) to the
environment's **ingest**: the released [`otlp-collector-oidc`](https://github.com/vcheesbrough/otlp-collector-oidc)
image, unmodified, one instance per environment, on bored's own hostname — Traefik routes
`/v1/traces` and `/v1/logs` (the only paths the SPA uses; no gRPC, no client metrics) to it and
everything else to the app. The ingest
authenticates the bearer, stamps `user.id`/`user.name`, `deployment.environment.name` and
`telemetry_source=client`, drops any `service.name` other than `bored-spa`, and forwards to
`monitor-alloy` like the server's own export. Frontend code: [`frontend/src/telemetry/`](frontend/src/telemetry/mod.rs).

**The bearer.** The ingest reads only `Authorization: Bearer <access JWT>` with `telemetry:write`
and the browser client's audience. The SPA gets the session's current access token from
`GET /api/telemetry/token` (cookie sessions only, `no-store`) and reads it per export. On a `401`
it drops its cached token and **re-fetches it once**; a second `401` stops export for the session.
The re-fetch is not a forced OIDC refresh: the route returns the session's current token, which
the auth middleware rotates only inside its 60 s refresh window. So it recovers what a `401` most
often is — a cached token that outlived the session's rotation, or one that expired in the
browser's cache — and anything else (a provider or ingest misconfiguration) fails the second time
too and stops export, which is the point of the rule. Sessions that began before the scope existed
get `403` from the route until the next sign-in. The panic record carries the source location
only, never the panic message (free text that can quote user content).

**Configuration.** The app tells the SPA where to send in `/api/info`'s `telemetry` block, from
the optional `client-telemetry` config group; no endpoint (or auth disabled) means the SPA never
initialises OTLP:

```
/bored/{dev,prod}/server/client-telemetry/endpoint   # https://<bored host>
```

The ingest's own variables are a sibling of the `otel` layer, rendered the same way (by
`scripts/compose-up-ingest.sh`, which passes an allowlist of collector variables through
`deploy/ingest.env` and keeps every `OTEL_*` away from docker itself):

```
/bored/devops/{dev,prod}/ingest/OIDC_ISSUER_URL              # alias of server/oidc/issuer-url
/bored/devops/{dev,prod}/ingest/OIDC_AUDIENCE                # alias of server/oidc/client-id
/bored/devops/{dev,prod}/ingest/ALLOWED_SERVICE_NAMES        # bored-spa
/bored/devops/{dev,prod}/ingest/CLAIM_ATTRIBUTES             # sub=user.id,preferred_username=user.name
/bored/devops/{dev,prod}/ingest/CLIENT_RESOURCE_ATTRIBUTES   # deployment.environment.name=<env>,telemetry_source=client
/bored/devops/{dev,prod}/ingest/OTEL_EXPORTER_OTLP_ENDPOINT  # alias of /observability/otlp-endpoint
/bored/devops/{dev,prod}/ingest/OTEL_EXPORTER_OTLP_PROTOCOL  # http/protobuf
/bored/devops/{dev,prod}/ingest/OTEL_RESOURCE_ATTRIBUTES     # the ingest's own: …,telemetry_source=otlp
/bored/devops/{dev,prod}/ingest/LOG_OUTPUT                   # otlp
```

**Never in the deploy gate.** The ingest is its own compose project (`deploy/otlp-ingest.yml`,
`<project>-ingest`), started after the app without `--wait`; a failure to start it is a warning in
the deploy log. **Stopping that project is the emergency stop**: the SPA drops its telemetry and
carries on. Client metrics are refused (`ALLOWED_METRIC_NAMES` empty).

## Local development

```bash
# A Postgres for the backend (any 16.x works; this is the one CI and prod run)
docker run -d --name bored-pg -e POSTGRES_USER=bored -e POSTGRES_PASSWORD=dev -e POSTGRES_DB=bored \
  -p 5432:5432 postgres:16-alpine

# Backend (plain HTTP on :3000 by default; anonymous auth when oidc.issuer-url is unset)
BORED__DATABASE__HOST=localhost BORED__DATABASE__PASSWORD=dev cargo run -p backend

# Backend tests: each test creates its own database on the server DATABASE_URL names.
# with-test-postgres.sh starts (and afterwards removes) a throwaway server for them.
./scripts/with-test-postgres.sh cargo test -p backend -p shared

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
