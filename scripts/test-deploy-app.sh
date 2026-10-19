#!/bin/sh
# Tests for scripts/deploy-app.sh (card #471).
#
# deploy-app.sh is copied into a scratch tree beside stand-ins for the two
# scripts it runs. Each stand-in drains stdin, as `docker compose` does, and
# records what it was called with, so the test sees the order, the arguments,
# and whether the caller's stdin survived.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORK_DIR="$(mktemp -d)"
# shellcheck disable=SC2064  # expand WORK_DIR now, not at trap time
trap "rm -rf '$WORK_DIR'" EXIT
mkdir -p "$WORK_DIR/scripts"
cp "$SCRIPT_DIR/deploy-app.sh" "$WORK_DIR/scripts/"

for name in migrate-from-surreal compose-up-with-otel; do
  cat > "$WORK_DIR/scripts/$name.sh" <<FAKE
#!/bin/sh
cat > /dev/null
echo "$name \$*" >> "$WORK_DIR/calls"
exit "\$(cat "$WORK_DIR/$name.status" 2>/dev/null || echo 0)"
FAKE
  chmod +x "$WORK_DIR/scripts/$name.sh"
done

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

# --- order and arguments, and the caller's stdin left alone -------------------
: > "$WORK_DIR/calls"
left=$(printf 'echo the-rest-of-the-step\n' | {
  sh "$WORK_DIR/scripts/deploy-app.sh" -p bored-dev -f deploy/docker-compose.yml > /dev/null 2>&1
  cat
})
expected="migrate-from-surreal -p bored-dev -f deploy/docker-compose.yml
compose-up-with-otel -p bored-dev -f deploy/docker-compose.yml up -d --pull always --wait --wait-timeout 120"
if [ "$(cat "$WORK_DIR/calls")" = "$expected" ]; then
  pass "the import check runs first, then compose up, with the caller's arguments"
else
  fail "calls: $(cat "$WORK_DIR/calls")"
fi
if [ "$left" = "echo the-rest-of-the-step" ]; then
  pass "neither half reads the caller's stdin"
else
  fail "stdin was consumed: left '$left'"
fi

# --- a failed import check stops before compose up ----------------------------
: > "$WORK_DIR/calls"
echo 1 > "$WORK_DIR/migrate-from-surreal.status"
status=0
sh "$WORK_DIR/scripts/deploy-app.sh" -p x < /dev/null > /dev/null 2>&1 || status=$?
if [ "$status" = 1 ] && ! grep -q compose-up-with-otel "$WORK_DIR/calls"; then
  pass "a failed import check stops the deploy before compose up"
else
  fail "status $status, calls: $(cat "$WORK_DIR/calls")"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all deploy-app.sh tests passed"
