# Agent guide — bored

This file is the single source of truth for AI agents and assistants working in this repo. It is read natively by Cursor, Claude Code, Codex, Aider, Jules, Amp, etc. Project overview, stack, and deploy details live in [`README.md`](README.md); this file is only the working-rules layer that used to be split across `.cursor/rules/*.mdc`.

If you change a rule below, change it here — there is no parallel copy.

---

## 1. Kanban / bored card workflow

When this project (or you) uses **Kanban cards** (e.g. bored) as the task queue:

1. **One card at a time** unless the user explicitly says otherwise (no parallel cards unless told).

2. **Start work:** As soon as you pick up a card, **move it to the In Progress column** using **bored MCP** (`list_columns` → **`move_card`**). Resolve the card with **`get_card_by_number`** when you only know `#N`.

3. **Iteration in the card title (this repo):** Bored cards have **no separate title field** — the board shows the **first markdown `#` heading** in **`body`**.
   - **Todo / backlog:** Use a **plain descriptive** heading only — **do not** write **`# Iteration N — …`** yet (**N** is unknown until work starts).
   - **In progress:** Right after **`move_card`** into **In progress**, **`update_card`** so the first `#` line becomes **`# Iteration N — …`** where **N** is the iteration you are committing to for this card (same **N** as **`feat/iteration-N-…`** / workspace **`Cargo.toml`** minor **1.N.x** at **start of work**). One prefix; avoid doubling.

4. **Reconcile with reality:** **Compare the card body to the current source tree**, **replan** if scope or facts drifted, then **`update_card`** so the card stays accurate (acceptance, files, out-of-scope notes). Preserve **`# Iteration N — …`** once set; if **N** changes mid-flight, **`update_card`** with the new heading.

5. **Branches:** After **N** is fixed (**In progress** per §3), implement on **`feat/iteration-N-short-slug`** from **`main`** (repo default trunk).
   **Never** create follow-on branches **from** the feature branch — always branch from **`main`**, one iteration branch per card.

6. **Ship:** When the **PR is merged**, **move the card to Done** via bored MCP (**`move_card`** into the Done column).

### Bored — MCP only

- Use **bored MCP tools** for **all** board/column/card reads and writes: `list_boards`, `get_board`, `list_columns`, `list_cards`, `get_card`, **`get_card_by_number`**, `create_card`, **`update_card`**, **`move_card`**, `delete_*`, `reorder_columns`, etc.
- **Do not** call the bored HTTP API with `curl`, scripts, or ad-hoc clients unless MCP is broken or unavailable — then say so once, fall back briefly, and still obey **one card**, trunk-based branches, **no stacking branches**.
- **Default endpoint:** **`https://bored.desync.link`** with scope **`bored:prod:access`** unless the user instructs otherwise. Rebuild **`cargo build -p mcp --release`** when MCP **code** changes. Do not add **`bored-dev`** / duplicate **`bored`** server entries on your own initiative.

---

## 2. Woodpecker / CI after every push

When this repo has just been pushed (or the user asks to verify CI):

1. **Confirm pipeline outcome for that commit** (Woodpecker reports to GitHub status).
   - From repo root, after push:
     - `SHA=$(git rev-parse HEAD)` and
       `gh api repos/vcheesbrough/bored/commits/$SHA/status --jq '.state'`
       → expect `success`.
     - Optional detail:
       `gh api repos/vcheesbrough/bored/commits/$SHA/status --jq '.statuses[] | "\(.context): \(.state)"'`

2. **If anything failed**, reproduce CI locally using [.woodpecker/build.yml](.woodpecker/build.yml):
   - `docker build -t bored:ci-local .` (rustfmt / clippy / tests / trunk all run inside the Dockerfile).
   - `TEST_IMAGE=bored:ci-local docker compose -f e2e/docker-compose.test.yml up --build --force-recreate --abort-on-container-exit --exit-code-from playwright`

3. **Fix failures** in-repo (narrow patches), commit, push, then **poll status again** until green.

If `gh` is unavailable or the status is `pending`, say so once and ask whether to wait/retry or use the Woodpecker UI. Do not invent a result.

---

## 3. MCP — non-negotiable obligations

These apply regardless of which agent / IDE you are running under:

- **First move on MCP failure:** execute the **canonical fix checklist** in §4 **before** improvising (no partial env merges, no guess-and-check JSON churn).
- **No restart theatre:** batch related edits; give **one** explicit outcome — **MCP reload** *or* **full quit** — not a vague loop of both.
- **No contradicting this file** unless the user **explicitly** overrides (e.g. another API host or scope).
- **Never paste secrets** from `~/.cursor/mcp.json` / `~/.claude.json` / any other config into chat.

---

## 4. Cursor MCP — canonical fix (remember forever)

**When Cursor MCP fails** (e.g. `bored` won't connect, `MCP_CLIENT_ID required when OIDC_TOKEN_URL is set`, tools missing after spawn):

1. **Make Cursor match Claude Code.** Copy `mcpServers` from `~/.claude.json` into `~/.cursor/mcp.json` **wholesale** (same `command` / `args` / `env` per server). Add `"type": "stdio"` on each server if Claude omits it. **Do not** hand-merge one field at a time — that has historically recreated the broken state.

2. **OAuth must arrive atomically for `bored-mcp`.** `OIDC_TOKEN_URL`, `MCP_CLIENT_ID`, `MCP_CLIENT_SECRET`, `MCP_SCOPE`, `BORED_API_URL` must all be present together in `env` (Claude's layout), **or** all come from a single `envFile` Cursor reliably loads — never only token URL in inline `env` while client id lives only in `envFile` (Cursor merge order caused `NotPresent` + panic).

3. **`command`:** Use an **absolute path** to `bored-mcp` (release binary path). Don't rely on `${userHome}` in `command` unless you've confirmed Cursor expands it. The repo ships [`./.cursor/run-bored-mcp.sh`](.cursor/run-bored-mcp.sh) as a stable launcher that resolves the binary under the workspace's custom `target-dir`.

4. **Secrets:** If `~/.cursor/mcp.json` holds tokens → `chmod 600`. Never paste those values into chat.

5. **One reload boundary:** After changing MCP JSON, tell the user **once** whether **MCP reload** or **full quit Cursor** is needed; batch edits so they aren't restart-looping for the same incident.

**Other machines / other clones:** Sync `~/.cursor/rules/` + `~/.cursor/mcp.json` via dotfiles or manual copy. The workspace `.cursor/mcp.json` is **gitignored**; [`.cursor/mcp.json.example`](.cursor/mcp.json.example) stays empty — real config comes from a Claude → Cursor sync, not a committed snippet with secrets.

---

## 5. Tool-specific helpers shipped in this repo

These files are intentionally per-tool and stay where they are. AGENTS.md just points at them:

| Path | Tool | Purpose |
|---|---|---|
| [`.cursor/run-bored-mcp.sh`](.cursor/run-bored-mcp.sh) | Cursor | stdio launcher for `bored-mcp` that resolves the binary under `~/.cargo/targets/bored/{debug,release}` (the workspace uses a custom Cargo target dir). |
| [`.cursor/mcp.json.example`](.cursor/mcp.json.example) | Cursor | Empty placeholder; the real `.cursor/mcp.json` is gitignored and synced from `~/.claude.json`. |
| [`.claude/watch-woodpecker.js`](.claude/watch-woodpecker.js) | Claude Code | PostToolUse hook that polls the latest Woodpecker pipeline after a push and emits a summary as `additionalContext`. Implements §2 automatically. Set `WOODPECKER_TOKEN` for authenticated log access. |

The `.claude/` folder itself is gitignored apart from this hook script; per-machine Claude Code settings live in `.claude/settings.local.json` (also gitignored).
