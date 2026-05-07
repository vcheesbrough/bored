#!/usr/bin/env bash
# Cursor MCP stdio launcher for bored-mcp. Workspace uses a custom Cargo target-dir
# (.cargo/config.toml → ~/.cargo/targets/bored); ${workspaceFolder}/target/debug/… is wrong.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BASE="${HOME}/.cargo/targets/bored"
BIN=""
for PROFILE in debug release; do
  CAND="${BASE}/${PROFILE}/bored-mcp"
  if [[ -x "$CAND" ]]; then
    BIN="$CAND"
    break
  fi
done

if [[ -z "$BIN" ]]; then
  echo "bored-mcp: not found under ${BASE}/{debug,release}; running cargo build -p mcp…" >&2
  cargo build -p mcp >&2
  BIN="${BASE}/debug/bored-mcp"
fi

exec "$BIN"
