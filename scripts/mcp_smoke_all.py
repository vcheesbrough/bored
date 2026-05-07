#!/usr/bin/env python3
"""
Full bored MCP stdio smoke test (NDJSON / newline-delimited JSON).

Exercises: initialize → notifications/initialized → tools/list →
tools/call get_card_by_number → tools/call list_boards.

Requires OAuth client-credentials in the environment (same as bored-mcp):
  OIDC_TOKEN_URL=https://auth.desync.link/application/o/token/
  MCP_CLIENT_ID / MCP_CLIENT_SECRET — from Authentik OAuth2 provider for MCP (slug issuer …/bored-mcp/)
  Optional: MCP_SCOPE=bored:dev:access (must satisfy bored-dev REQUIRED_SCOPE)

Optional: repo-root .env (gitignored) with those keys — pass --env-file.

Usage:
  python3 scripts/mcp_smoke_all.py
  python3 scripts/mcp_smoke_all.py --card-number 130 --env-file .env
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path


def load_env_file(path: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    text = path.read_text(encoding="utf-8")
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        key, _, val = line.partition("=")
        key = key.strip()
        val = val.strip().strip("'").strip('"')
        out[key] = val
    return out


def ndjson_write(proc: subprocess.Popen, obj: dict) -> None:
    assert proc.stdin
    proc.stdin.write((json.dumps(obj, separators=(",", ":")) + "\n").encode())
    proc.stdin.flush()


def ndjson_read(proc: subprocess.Popen) -> dict | None:
    assert proc.stdout
    line = proc.stdout.readline()
    if not line:
        return None
    return json.loads(line.decode())


def extract_text_content(result: dict) -> str:
    parts: list[str] = []
    for block in (result.get("content") or []) if isinstance(result, dict) else []:
        if isinstance(block, dict) and block.get("type") == "text":
            parts.append(str(block.get("text", "")))
    return "".join(parts)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[1])
    ap.add_argument("--card-number", type=int, default=130)
    ap.add_argument("--env-file", type=Path, default=None, help="dotenv file (default: <repo>/.env if present)")
    args = ap.parse_args()

    repo = args.repo_root
    env_file = args.env_file
    if env_file is None:
        candidate = repo / ".env"
        env_file = candidate if candidate.is_file() else None

    child_env = os.environ.copy()
    child_env.setdefault("BORED_API_URL", "https://bored-dev.desync.link")
    if env_file is not None:
        extra = load_env_file(env_file)
        child_env.update(extra)

    need = ["OIDC_TOKEN_URL", "MCP_CLIENT_ID", "MCP_CLIENT_SECRET"]
    missing = [k for k in need if not child_env.get(k)]
    if missing:
        print(
            "Missing OAuth env for bored-mcp (set in shell or .env):",
            ", ".join(missing),
            file=sys.stderr,
        )
        return 2

    launcher = repo / ".cursor" / "run-bored-mcp.sh"
    if not launcher.is_file():
        print("Launcher not found:", launcher, file=sys.stderr)
        return 2

    proc = subprocess.Popen(
        [str(launcher)],
        cwd=str(repo),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=child_env,
    )

    def call_tools(name: str, arguments: dict, req_id: int) -> dict | None:
        ndjson_write(
            proc,
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            },
        )
        return ndjson_read(proc)

    try:
        ndjson_write(
            proc,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "mcp_smoke_all", "version": "1.0"},
                },
            },
        )
        init = ndjson_read(proc)
        if not init or "result" not in init:
            print("initialize failed:", init, file=sys.stderr)
            return 1
        print("initialize: ok — server:", init["result"].get("serverInfo"))

        ndjson_write(proc, {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})

        ndjson_write(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        lst = ndjson_read(proc)
        if not lst or "result" not in lst:
            print("tools/list failed:", lst, file=sys.stderr)
            return 1
        names = sorted(t["name"] for t in lst["result"].get("tools", []) if "name" in t)
        print(f"tools/list: ok — {len(names)} tools")

        for tool_name in ("get_card_by_number", "list_boards"):
            if tool_name not in names:
                print("missing tool:", tool_name, file=sys.stderr)
                return 1

        r3 = call_tools("get_card_by_number", {"number": int(args.card_number)}, 3)
        if not r3:
            print("no response for get_card_by_number", file=sys.stderr)
            return 1
        if r3.get("error"):
            print("get_card_by_number error:", json.dumps(r3["error"], indent=2))
            return 1
        text = extract_text_content(r3.get("result") or {})
        try:
            card = json.loads(text)
        except json.JSONDecodeError:
            print("get_card_by_number raw:", text[:3000])
            return 1
        body = card.get("body", "")
        preview = body if len(body) <= 2500 else body[:2500] + "\n… [truncated]"
        print(f"get_card_by_number({args.card_number}): ok — id={card.get('id')} number={card.get('number')}")
        print("--- body preview ---")
        print(preview)
        print("--- end preview ---")

        r4 = call_tools("list_boards", {}, 4)
        if not r4 or r4.get("error"):
            print("list_boards failed:", r4, file=sys.stderr)
            return 1
        boards_raw = extract_text_content(r4.get("result") or {})
        try:
            boards = json.loads(boards_raw)
        except json.JSONDecodeError:
            print("list_boards bad JSON:", boards_raw[:500], file=sys.stderr)
            return 1
        print(f"list_boards: ok — {len(boards)} board(s)")
        for b in boards[:5]:
            print(f"  - {b.get('name')} ({b.get('id')})")

    finally:
        try:
            _, err_bytes = proc.communicate(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()
            _, err_bytes = proc.communicate()
        err = err_bytes.decode(errors="replace") if err_bytes else ""
        if err.strip():
            print("\n--- bored-mcp stderr (tail) ---\n", err[-2000:], sep="")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
