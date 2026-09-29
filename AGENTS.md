# Agent guide — bored

This file holds the **bored-specific** working rules. The machine-global rules
common to every repo — Kanban/bored workflow, iteration & versioning defaults,
branching & git safety, CI-after-push, PR self-review + comment loop, test
coverage, and MCP/secrets discipline — live in the shared **agent-shared
baseline**, imported on this machine via `~/.codex/AGENTS.md` and
`~/.claude/CLAUDE.md`. **Read that baseline first;** this file only records what
is specific to bored or overrides the baseline.

If you change a bored-specific rule, change it here — there is no parallel copy.
Cross-repo rules change in `agent-shared`, not here.

Project overview, stack, and deploy details live in [`README.md`](README.md);
this file is only the working-rules layer that used to be split across
`.cursor/rules/*.mdc`.

---

## Repository context

| Item | Value |
| --- | --- |
| **Remote / `gh` repo** | `vcheesbrough/bored` |
| **Trunk** | `main` |
| **Kanban board** | the bored product board on `https://bored.desync.link` (slug `bored-project`; resolve via `list_boards`) |
| **Phase** | post-MVP — iteration semver is `1.N.x` (baseline §2 post-MVP form) |

Iteration `N` matches the workspace **`Cargo.toml`** minor (`1.N.x`) at start of
work; feature branches are `feat/iteration-N-short-slug` from `main`. Start a
card with the **`start-iteration`** skill (baseline §1–§2); bored is post-MVP so
versions are `1.N.x`.

---

## 1. CI after every push (repo commands)

Run the **`ci-watch`** skill (baseline §4). Skill parameters:

- `OWNER=vcheesbrough`, `REPO=bored`.
- **Reproduce failures locally** from [.woodpecker/build.yml](.woodpecker/build.yml):
  - `docker build --secret id=github_token,env=GITHUB_TOKEN -t bored:ci-local .` (rustfmt / clippy / tests / trunk all
    run inside the Dockerfile). The `github_token` secret fetches the private `sovereign-config-provider` git
    dependency (see [README.md § Runtime configuration](README.md#runtime-configuration)) — export
    `GITHUB_TOKEN` first, per **Getting `GITHUB_TOKEN`** below.
  - `TEST_IMAGE=bored:ci-local docker compose -f e2e/docker-compose.test.yml up --build --force-recreate --abort-on-container-exit --exit-code-from playwright`

The [`.claude/watch-woodpecker.js`](.claude/watch-woodpecker.js) PostToolUse
hook automates the polling (set `WOODPECKER_TOKEN` for authenticated logs).

### Getting `GITHUB_TOKEN`

An ordinary `gh` login is enough — **no dedicated PAT is required.** The
`gh` OAuth token carries the `repo` scope, which authorises the private
`vcheesbrough/sovereign-config` fetch (verified 2026-09-18: `repo` present in
`x-oauth-scopes`, `200` on the repo, `git ls-remote` succeeds through the
`x-access-token` rewrite). Extract it with whichever line matches your `gh`:

```bash
# gh >= 2.5.0
export GITHUB_TOKEN="$(gh auth token)"

# gh < 2.5.0 — including the 2.4.0 that Ubuntu ships, where `gh auth token`
# does not exist and a command substitution would silently capture its error
# message instead of a token.
export GITHUB_TOKEN="$(gh auth status --show-token 2>&1 | sed -n 's/.*Token: //p')"
```

Check it took: `[ -n "$GITHUB_TOKEN" ] && echo "${#GITHUB_TOKEN} chars"` should
print ~40. If you would rather not use the `gh` token, any classic PAT with the
`repo` scope works.

Get this wrong and the build now says so:
[`scripts/docker-git-credential.sh`](scripts/docker-git-credential.sh) rejects a
secret that is not a plausible credential, naming `github_token` rather than
failing later with a misleading clone/revision error against `sovereign-config`.

### Without a token

If you genuinely cannot get one, run the same checks the image runs — these
need only your own git access to the private dependency, not the build secret:

```bash
cargo fmt -p backend -p shared --check                # Dockerfile backend-builder
cargo clippy -p backend -p shared -- -D warnings      # Dockerfile backend-builder
cargo test -p backend -p shared                       # Dockerfile backend-builder
cargo test -p frontend                                # HOST target, not wasm32 — see below
sh scripts/test-docker-git-credential.sh              # Dockerfile frontend-builder
```

`cargo test -p frontend` is deliberately untargeted: `frontend` is a
`[[bin]]`-only crate of plain logic tests, and a `wasm32-unknown-unknown` test
binary has no runner in the image. That is the full set — the Dockerfile runs
no wasm32 clippy and no `fmt`/`clippy` over `frontend` or `mcp`.

These do **not** run without a token: the `docker build` itself, `trunk build`,
and therefore the **whole** e2e compose run, which needs the
`TEST_IMAGE=bored:ci-local` image that build produces. Say so explicitly in the
PR rather than implying the e2e suite passed.

---

## 2. Cursor MCP — canonical fix (remember forever)

**When Cursor MCP fails** (e.g. `bored` won't connect,
`MCP_CLIENT_ID required when OIDC_TOKEN_URL is set`, tools missing after spawn),
run this checklist **before** improvising (baseline §7):

1. **Make Cursor match Claude Code.** Copy `mcpServers` from `~/.claude.json`
   into `~/.cursor/mcp.json` **wholesale** (same `command` / `args` / `env` per
   server). Add `"type": "stdio"` on each server if Claude omits it. **Do not**
   hand-merge one field at a time — that has historically recreated the broken
   state.

2. **OAuth must arrive atomically for `bored-mcp`.** `OIDC_TOKEN_URL`,
   `MCP_CLIENT_ID`, `MCP_CLIENT_SECRET`, `MCP_SCOPE`, `BORED_API_URL` must all
   be present together in `env` (Claude's layout), **or** all come from a single
   `envFile` Cursor reliably loads — never only token URL in inline `env` while
   client id lives only in `envFile` (Cursor merge order caused `NotPresent` +
   panic).

3. **`command`:** Use an **absolute path** to `bored-mcp` (release binary path).
   Don't rely on `${userHome}` in `command` unless you've confirmed Cursor
   expands it. The repo ships [`./.cursor/run-bored-mcp.sh`](.cursor/run-bored-mcp.sh)
   as a stable launcher that resolves the binary under the workspace's custom
   `target-dir`.

4. **Secrets:** If `~/.cursor/mcp.json` holds tokens → `chmod 600`. Never paste
   those values into chat.

5. **One reload boundary:** After changing MCP JSON, tell the user **once**
   whether **MCP reload** or **full quit Cursor** is needed; batch edits so they
   aren't restart-looping for the same incident.

**Other machines / other clones:** Sync `~/.cursor/rules/` + `~/.cursor/mcp.json`
via dotfiles or manual copy. The workspace `.cursor/mcp.json` is **gitignored**;
[`.cursor/mcp.json.example`](.cursor/mcp.json.example) stays empty — real config
comes from a Claude → Cursor sync, not a committed snippet with secrets.

Rebuild `cargo build -p mcp --release` when MCP **code** changes. Do not add
`bored-dev` / duplicate `bored` server entries on your own initiative.

---

## 3. Tool-specific helpers shipped in this repo

These files are intentionally per-tool and stay where they are:

| Path | Tool | Purpose |
|---|---|---|
| [`.cursor/run-bored-mcp.sh`](.cursor/run-bored-mcp.sh) | Cursor | stdio launcher for `bored-mcp` that resolves the binary under `~/.cargo/targets/bored/{debug,release}` (the workspace uses a custom Cargo target dir). |
| [`.cursor/mcp.json.example`](.cursor/mcp.json.example) | Cursor | Empty placeholder; the real `.cursor/mcp.json` is gitignored and synced from `~/.claude.json`. |
| [`.claude/watch-woodpecker.js`](.claude/watch-woodpecker.js) | Claude Code | PostToolUse hook that polls the latest Woodpecker pipeline after a push and emits a summary as `additionalContext`. Implements §1 automatically. Set `WOODPECKER_TOKEN` for authenticated log access. |

The `.claude/` folder itself is gitignored apart from this hook script; per-machine
Claude Code settings live in `.claude/settings.local.json` (also gitignored).

---

## 4. Runtime configuration — write it, don't pipeline-edit it

Bored's runtime config (OIDC, the browser session cookie key, observability settings)
lives in **sovereign-config**, not in `.woodpecker/build.yml` env blocks — see
[README.md § Runtime configuration](README.md#runtime-configuration) for the full
layering model and subtree layout.

- **Change a value** with the sovereign-config MCP/CLI (`put` / `put_secret` against
  `/bored/{dev,prod}/server/{oidc,session,observability}`), then redeploy. Do **not**
  add a value back into `.woodpecker/build.yml` or `deploy/docker-compose.yml` — the
  whole point of card #288 was collapsing those inline blocks into one access-URL
  secret.
- **`server/*`** (`http-port`, `tls-cert`, `tls-key`, `static-dir`, `database-path`) is
  the one group that stays out of sovereign-config — it's image-internal, set via
  `Dockerfile` `ENV BORED__SERVER__*` or in-code defaults.
- **Rotating an access URL:** sovereign-config `rotate_connection` + rewrite the
  Woodpecker secret (`bored_{dev,prod}_sovereign_access_url`, itself stored in
  sovereign-config) + redeploy. No app change, no image rebuild.
- **Bumping the sovereign-config server:** the provider dependency in
  `backend/Cargo.toml` negotiates a protocol version on connect, so a server upgrade
  needs no bored rebuild. Bump the `tag` to pick up client fixes, and before the
  server retires the protocol version the pinned provider speaks.

---

## Observability

The contract is the machine-global **`observability`** skill (and its
`references/rust.md` for the crate set). The backend meets its §1 bar since
card #415: all three signals over OTLP `http/protobuf`, one resource on every
signal, a span per request and per call that leaves the process, correlated
logs, RED + saturation metrics, `bored.build.info`, telemetry optional at
runtime, health separate from it. How it is wired: README § Telemetry. The
tests that protect it live in `backend/src/observability/tests.rs` — **do not
weaken them.**

**What is still short of the contract, deliberately:**

- **No exemplars.** The Rust SDK does not implement them; metric-to-trace
  correlation is by time range and labels.
- **No dropped-batch counter.** The SDK exports nothing about itself. Decision:
  record the gap rather than wrap the exporter. Export failures are logged by
  the SDK on its own `opentelemetry*` target at `warn`, rate-limited, on stdout
  (which Alloy still collects), and never on the OTLP log path.
- **`code.module.name`** on spans comes from `tracing-opentelemetry`'s location
  option and is not a semantic-convention name; the span-key test allowlists
  exactly that one key.
- **Logs are exported twice** — over OTLP and as stdout JSON, which Alloy
  still collects (`log_source="docker"`). The stdout copy is crash-safe and is
  what `docker logs` and e2e read; whether Alloy keeps shipping it is the
  platform's decision.
- **An inbound `traceparent` is trusted, sampling flag included.** The
  sampler is parent-based (the contract's default), so a client that sends
  `traceparent: …-00` through Traefik gets its requests' spans dropped, and it
  chooses the trace id its log lines are correlated under. Logs and metrics
  still record every request, so this suppresses the trace record, not the
  fact of the request. Accepted: sampling policy is the platform's to set
  (skill §2), and the trust boundary belongs at the edge — Traefik could drop
  or re-root inbound context — not in each product. Revisit with a
  non-default sampler in the `otel` layer if the trace record ever has to
  withstand a hostile client.
- **Only `http/protobuf`.** `grpc` and `http/json` are valid OTLP protocols but
  this build carries only the http/protobuf client, so startup rejects them.
- **The one duplicated fact:** `observability.environment` (for `/api/info`,
  needed with telemetry off) and `deployment.environment.name` in
  `OTEL_RESOURCE_ATTRIBUTES`. The deploy states both for one `APP_ENV`, and the
  backend refuses to start if they disagree.

**Rules for every card:**

- **Decide telemetry per card.** Every card that changes behaviour records, on
  the card, whether it needs new or changed spans, metrics, log fields or
  correlation — "no change needed" is a decision to write down, not the
  default (skill §1).
- **Only `observability.rs` (and `observability/`) may name
  `opentelemetry_sdk`, `opentelemetry_otlp`, `tracing_opentelemetry` or
  `opentelemetry_appender_tracing`.** Product code uses `tracing`, the
  `opentelemetry` API crate and `observability::metrics`.
- **Metric labels come from enums** in `observability/metrics.rs`; identifiers,
  raw paths and user input go on span attributes, never labels. Span keys set
  at creation are literals inside `tracing` macros; they must be semconv names
  or `bored.`-prefixed.
- **One fact, one signal.** No per-request "request completed" line (the
  server span and the histogram are that record); no `debug!` per query (the db
  span is).
- **The redaction rule (card #366) covers every signal.** Tokens, cookies,
  client secrets, request bodies, query strings, SQL text and bound values stay
  out of log fields, span attributes and error messages — telemetry leaves the
  process and lands somewhere with different access control. A new log site
  describes an outbound or database error through `crate::redact`
  (`http_error`, `url`/`url_parts`, `identifier`, `variant_path`), never with
  the error's own `Display`. Spans follow the same rule: `url.path` and
  `url.full` carry no query, database spans carry a query *name*
  (`DbQuery`), and `error.type` is always an enum label.
- **New outbound calls and database calls get their spans by construction:**
  send HTTP through `http_client::send(Outbound::…, request)` and put
  `.traced(DbQuery::new(..))` before a SurrealDB call's `.await`.
- **Dashboards and alerts are opt-in** (skill §8) and not yet opted into; card
  #437 is where that decision is made.

### Client telemetry (the SPA, card #416)

The SPA exports its own traces and logs (`service.name=bored-spa`) over
OTLP/HTTP JSON to the environment's ingest — the released
`otlp-collector-oidc` image, one per environment, same-origin behind Traefik
(`/v1/`). Contract: the skill's `references/client-export.md`. Wiring:
README § Client telemetry. Tests: `frontend/src/telemetry/**/tests`,
`backend/src/routes/telemetry.rs`, `e2e/tests/telemetry.spec.ts`.

**Decisions and deviations, recorded:**

- **The bearer is the session's own access token** (D1), handed to the page by
  `GET /api/telemetry/token`. The browser providers carry a `telemetry:write`
  scope mapping (`authentik/`), login requests it, and the ingest's audience is
  the browser client id. Cost: the page can read a full bored API bearer for
  ≤ 15 min, and "who may send" is "who may use bored" (`bored-{env}-users`) —
  a per-user opt-out needs a separate telemetry provider. The "one refresh" a
  `401` gets is a *re-fetch* of the session's current token, not a forced OIDC
  refresh; it recovers a stale cached token, and a misconfiguration fails twice
  and stops export for the session.
- **Panics export their location only** (`code.file.path`, line, column), never
  the panic message — the card first allowed the message as the one exception
  to "no user content"; review withdrew it, since panics can format user data.
- **Identity is narrowed** to `user.id` and `user.name` (`CLAIM_ATTRIBUTES`,
  D2); the ingest does not stamp email or full name.
- **The unload flush is a `keepalive` fetch**, not `sendBeacon` (D3): a beacon
  cannot carry `Authorization`.
- **e2e reaches the ingest cross-origin** (CORS on) rather than through a
  Traefik edge; see card #449 (SSE events lost behind Traefik).

**Rules:**

- **Frontend failures go through `telemetry::error`** (or `error_detail`), not
  `leptos::logging::error!`. The console line is unchanged; the exported record
  carries only the fixed message, `error.type`, status and trace — never the
  error's text, which can quote user content.
- **Parents are explicit.** A new span takes its parent as a `SpanContext`
  argument; there is no ambient current span in the SPA. A REST call a screen
  load makes passes the load's context; a user action passes `None`.
- **New REST calls go through `api::send`**, which opens the `http.client`
  span and sends `traceparent`. Polled requests (the heartbeat) are not traced.
- **No client metrics**: the ingest's `ALLOWED_METRIC_NAMES` stays empty.

---

## 5. PR review — repo hooks

Run the **`pr-review-loop`** skill (baseline §5: self-review every PR you open,
then the one-comment-at-a-time triage loop). Repo parameters for the skill:

- `OWNER=vcheesbrough`, `REPO=bored`.
- **Review criteria:** the baseline five — correctness, security / OWASP, test
  coverage of the changed behaviour, versioning, scope. No repo-specific rubric
  file; `.woodpecker/pr-review-prompt.md` is dead and is **not** to be applied.
- **The local review is the only review this repo gets.** The Woodpecker
  remote-review pipeline is disabled (`pr-review.yml.disabled`), so nothing
  reviews a PR here except the `pr-self-review` subagent Part A spawns.
  Self-review is not optional.
- **Sanity check** before batching commits: `cargo check -p <crate>` (Rust) or
  `trunk build` (frontend).
