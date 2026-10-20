#!/usr/bin/env bash
# Test scripts/attach-sqltool-outpost.sh against a stub Authentik API.
#
# Why this exists: that script does a read-modify-write on the *shared* embedded
# outpost, whose provider list also holds every other service Authentik protects
# on this LAN. A bug that writes a short list detaches all of them, and the only
# place the script ever runs is a deploy against live infrastructure — so the
# failure would be discovered by someone unable to log in, not by CI.
#
# The stub records what it is asked to PATCH, so these are assertions about the
# bytes on the wire rather than about the exit code:
#
#   missing      → provider absent from the list → PATCHes existing + new
#   late-provider → the provider appears on the third lookup → waits, then attaches
#   present      → provider already attached     → no PATCH at all
#   no-provider  → blueprint has not applied yet → fails, no PATCH
#   no-outpost   → outpost name does not resolve → fails, no PATCH
#   lying-patch  → server reports it did not take → fails (read-back catches it)
#
# Ported from v-note's test of the same script: a
# repo-owned script CI points at shared infrastructure gets a test that runs
# without that infrastructure.

set -euo pipefail

cd "$(dirname "$0")/.."

PORT="${TEST_PORT:-18399}"
WORK=$(mktemp -d)
STUB_PID=""

cleanup() {
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# Written before the stub starts so the readiness probe below has a mode to read.
echo missing > "$WORK/mode"

python3 - "$PORT" "$WORK" <<'STUB' &
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, parse_qs

PORT = int(sys.argv[1])
WORK = sys.argv[2]

# The existing providers stand in for the eleven real ones. Any response that
# fails to carry all of them through is the outage this test exists to catch.
EXISTING = [1, 5, 7, 9]
NEW_PK = 42


def mode():
    with open(f"{WORK}/mode") as handle:
        return handle.read().strip()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _json(self, code, payload):
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        url = urlparse(self.path)
        current = mode()

        if url.path == "/api/v3/providers/proxy/":
            # Every lookup is counted, so a test can check how many were made.
            try:
                with open(f"{WORK}/lookup-count") as handle:
                    total = int(handle.read())
            except FileNotFoundError:
                total = 0
            with open(f"{WORK}/lookup-count", "w") as handle:
                handle.write(str(total + 1))
            if current == "no-provider":
                return self._json(200, {"results": []})
            if current == "late-provider":
                # The blueprint apply finishing a moment after the step
                # starts: absent on the first two lookups, then present.
                try:
                    with open(f"{WORK}/lookups") as handle:
                        seen = int(handle.read())
                except FileNotFoundError:
                    seen = 0
                with open(f"{WORK}/lookups", "w") as handle:
                    handle.write(str(seen + 1))
                if seen < 2:
                    return self._json(200, {"results": []})
            return self._json(200, {"results": [{"pk": NEW_PK, "name": "bored-sql-console-dev"}]})

        if url.path == "/api/v3/outposts/instances/":
            if current == "no-outpost":
                return self._json(200, {"results": []})

            providers = list(EXISTING)
            if current == "present":
                providers.append(NEW_PK)
            elif current == "lying-patch":
                # The PATCH is accepted but the read-back still lacks it.
                pass
            elif current == "dropped-others":
                # The failure every comment in this repo is about: our provider
                # attached, everyone else's gone. Returned only once the PATCH
                # has happened, so the pre-write read still sees a full list.
                # A *second* PATCH (the script's restore) does take, so the
                # test can check what the restore wrote and that it verified.
                try:
                    with open(f"{WORK}/patch-count") as handle:
                        count = int(handle.read())
                except FileNotFoundError:
                    count = 0
                if count == 1:
                    providers = [NEW_PK]
                elif count >= 2:
                    with open(f"{WORK}/patched") as handle:
                        providers = json.load(handle)["providers"]
            else:
                # After a successful patch the read-back reflects it.
                try:
                    with open(f"{WORK}/patched") as handle:
                        providers = json.load(handle)["providers"]
                except FileNotFoundError:
                    pass

            return self._json(
                200,
                {"results": [{"pk": "outpost-uuid", "name": "authentik Embedded Outpost",
                              "providers": providers}]},
            )

        return self._json(404, {"detail": "not found"})

    def do_PATCH(self):
        length = int(self.headers.get("Content-Length", 0))
        payload = json.loads(self.rfile.read(length) or b"{}")
        with open(f"{WORK}/patched", "w") as handle:
            json.dump(payload, handle)
        try:
            with open(f"{WORK}/patch-count") as handle:
                count = int(handle.read())
        except FileNotFoundError:
            count = 0
        with open(f"{WORK}/patch-count", "w") as handle:
            handle.write(str(count + 1))
        return self._json(200, {"pk": "outpost-uuid", **payload})


HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
STUB
STUB_PID=$!

for _ in $(seq 1 50); do
  curl -sS -o /dev/null "http://127.0.0.1:${PORT}/api/v3/providers/proxy/" 2>/dev/null && break
  sleep 0.1
done

failures=0

check() {
  if [ "$1" = "ok" ]; then
    echo "  ok   — $2"
  else
    echo "  FAIL — $2"
    failures=$((failures + 1))
  fi
}

run_mode() {
  echo "$1" > "$WORK/mode"
  rm -f "$WORK/patched" "$WORK/patch-count" "$WORK/lookups" "$WORK/lookup-count"
  set +e
  LOOKUP_ATTEMPTS="${LOOKUP_ATTEMPTS_OVERRIDE:-1}" \
  LOOKUP_DELAY=0 \
  AUTHENTIK_URL="http://127.0.0.1:${PORT}" \
  AUTHENTIK_TOKEN=stub-token \
  PROVIDER_NAME=bored-sql-console-dev \
    ./scripts/attach-sqltool-outpost.sh >"$WORK/out" 2>&1
  RC=$?
  set -e
}

echo "==> a missing provider is appended, and nothing else is dropped"
run_mode missing
check "$([ "$RC" -eq 0 ] && echo ok)" "exits 0 (rc=$RC)"
if [ -f "$WORK/patched" ]; then
  patched=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["providers"])' "$WORK/patched")
  check "$([ "$patched" = "[1, 5, 7, 9, 42]" ] && echo ok)" "PATCHes existing + new (got $patched)"
else
  check "" "a PATCH was sent"
fi

echo "==> an already-attached provider is a no-op"
run_mode present
check "$([ "$RC" -eq 0 ] && echo ok)" "exits 0 (rc=$RC)"
check "$([ ! -f "$WORK/patched" ] && echo ok)" "sends no PATCH at all"

echo "==> a provider the blueprint has not created yet fails loudly"
run_mode no-provider
check "$([ "$RC" -ne 0 ] && echo ok)" "exits non-zero (rc=$RC)"
check "$([ ! -f "$WORK/patched" ] && echo ok)" "sends no PATCH"

echo "==> a provider the blueprint apply creates a moment late is waited for"
LOOKUP_ATTEMPTS_OVERRIDE=5 run_mode late-provider
check "$([ "$RC" -eq 0 ] && echo ok)" "exits 0 once it appears (rc=$RC)"
check "$(grep -q 'not there yet' "$WORK/out" && echo ok)" "says it is waiting"
if [ -f "$WORK/patched" ]; then
  patched=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["providers"])' "$WORK/patched")
  check "$([ "$patched" = "[1, 5, 7, 9, 42]" ] && echo ok)" "then attaches it (got $patched)"
else
  check "" "then attaches it"
fi

echo "==> a provider that never appears fails once the attempts are spent"
LOOKUP_ATTEMPTS_OVERRIDE=3 run_mode no-provider
check "$([ "$RC" -ne 0 ] && echo ok)" "exits non-zero (rc=$RC)"
check "$([ "$(cat "$WORK/lookup-count")" = "3" ] && echo ok)" \
  "looks exactly LOOKUP_ATTEMPTS times (made $(cat "$WORK/lookup-count"))"
check "$(grep -q 'after 3 lookup' "$WORK/out" && echo ok)" "says how long it waited"
check "$([ ! -f "$WORK/patched" ] && echo ok)" "sends no PATCH"

echo "==> an unresolvable outpost fails loudly"
run_mode no-outpost
check "$([ "$RC" -ne 0 ] && echo ok)" "exits non-zero (rc=$RC)"
check "$([ ! -f "$WORK/patched" ] && echo ok)" "sends no PATCH"

echo "==> a PATCH that did not take is caught by the read-back"
run_mode lying-patch
check "$([ "$RC" -ne 0 ] && echo ok)" "exits non-zero (rc=$RC)"
check "$(grep -q 'still not attached' "$WORK/out" && echo ok)" "says the provider is still not attached"

echo "==> a PATCH that attached ours and dropped everyone else's is caught"
# The case that actually matters, and the one a read-back asserting only
# "our provider is present" passes with a cheerful count. Without this mode
# nothing here exercises "the list came back short" at all.
run_mode dropped-others
check "$([ "$RC" -ne 0 ] && echo ok)" "exits non-zero (rc=$RC)"
check "$(grep -q 'lost providers it had before' "$WORK/out" && echo ok)" \
  "says the outpost lost providers"
check "$(grep -q 'before=\[1,5,7,9\]' "$WORK/out" && echo ok)" \
  "prints the list it had before, so it can be restored"
restored=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["providers"])' "$WORK/patched")
check "$([ "$restored" = "[1, 5, 7, 9, 42]" ] && echo ok)" \
  "writes the lost providers back (got $restored)"
check "$(grep -q 'restored; the outpost serves' "$WORK/out" && echo ok)" \
  "verifies the restore took"

if [ "$failures" -gt 0 ]; then
  echo
  echo "attach-sqltool-outpost validation FAILED ($failures check(s))"
  exit 1
fi
echo
echo "attach-sqltool-outpost OK"
