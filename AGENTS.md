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
    `GITHUB_TOKEN` first (e.g. a `gh auth token`-equivalent PAT with repo read access).
  - `TEST_IMAGE=bored:ci-local docker compose -f e2e/docker-compose.test.yml up --build --force-recreate --abort-on-container-exit --exit-code-from playwright`

The [`.claude/watch-woodpecker.js`](.claude/watch-woodpecker.js) PostToolUse
hook automates the polling (set `WOODPECKER_TOKEN` for authenticated logs).

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
  `backend/Cargo.toml` is pinned to the server's running version and fails closed on
  protocol mismatch — bump the `tag` and rebuild when the server upgrades.

---

## 5. PR review — repo hooks

Run the **`pr-review-loop`** skill (baseline §5: self-review every PR you open,
then the one-comment-at-a-time triage loop). Repo parameters for the skill:

- `OWNER=vcheesbrough`, `REPO=bored`.
- **Review rubric:** apply [`.woodpecker/pr-review-prompt.md`](.woodpecker/pr-review-prompt.md)
  **verbatim** — the same rubric the Woodpecker [`pr-review.yml`](.woodpecker/pr-review.yml)
  pipeline runs (Correctness, Security, OWASP Top 10, Tests, Versioning, General).
  Self-review regardless of whether the automated pipeline succeeded.
- **Sanity check** before batching commits: `cargo check -p <crate>` (Rust) or
  `trunk build` (frontend).
