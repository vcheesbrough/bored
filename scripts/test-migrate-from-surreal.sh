#!/bin/sh
# Tests for scripts/migrate-from-surreal.sh (card #471).
#
# Plain POSIX sh, run by the Dockerfile like the other script tests. A fake
# `docker` first on PATH logs every call and answers `--check-empty` with the
# exit status the case sets, so each path of the script is driven without a
# daemon.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
UNDER_TEST="$SCRIPT_DIR/migrate-from-surreal.sh"

WORK_DIR="$(mktemp -d)"
# shellcheck disable=SC2064  # expand WORK_DIR now, not at trap time
trap "rm -rf '$WORK_DIR'" EXIT
mkdir -p "$WORK_DIR/bin"

cat > "$WORK_DIR/bin/docker" <<FAKE
#!/bin/sh
echo "\$*" >> "$WORK_DIR/calls"
env | grep '^OTEL_' >> "$WORK_DIR/otel" || true
case "\$*" in
  *--check-empty*) exit "\$(cat "$WORK_DIR/check-status")" ;;
  *--surreal*) exit "\$(cat "$WORK_DIR/import-status")" ;;
esac
exit 0
FAKE
chmod +x "$WORK_DIR/bin/docker"

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

# Run the script with the given check and import statuses; echo its status.
run_case() {
  : > "$WORK_DIR/calls"
  : > "$WORK_DIR/otel"
  echo "$1" > "$WORK_DIR/check-status"
  echo "$2" > "$WORK_DIR/import-status"
  status=0
  PATH="$WORK_DIR/bin:$PATH" BORED_CONTAINER_NAME=bored-dev \
    OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4318 \
    sh "$UNDER_TEST" -p bored-dev -f deploy/docker-compose.yml > "$WORK_DIR/out" 2>&1 || status=$?
  echo "$status"
}

# --- already migrated: nothing is stopped, nothing imported -------------------
status=$(run_case 3 0)
if [ "$status" = 0 ] && ! grep -q 'stop\|--surreal' "$WORK_DIR/calls"; then
  pass "a non-empty database skips the import and leaves the backend running"
else
  fail "skip path: status $status, calls: $(cat "$WORK_DIR/calls")"
fi
if grep -q 'compose -p bored-dev -f deploy/docker-compose.yml up -d --wait --wait-timeout 120 bored-db' "$WORK_DIR/calls"; then
  pass "postgres is brought up first, with the caller's project arguments"
else
  fail "db start: $(cat "$WORK_DIR/calls")"
fi

# --- empty: stop the old backend, then import ---------------------------------
status=$(run_case 0 0)
stop_line=$(grep -n 'stop -t 15 bored-dev' "$WORK_DIR/calls" | cut -d: -f1)
import_line=$(grep -n 'run --rm --no-deps surreal-migrate --surreal /surreal/bored.db' "$WORK_DIR/calls" | cut -d: -f1)
if [ "$status" = 0 ] && [ -n "$stop_line" ] && [ -n "$import_line" ] && [ "$stop_line" -lt "$import_line" ]; then
  pass "an empty database stops the old backend and then imports"
else
  fail "import path: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- a failed import fails the deploy -----------------------------------------
status=$(run_case 0 1)
if [ "$status" != 0 ]; then
  pass "a failed import fails the script"
else
  fail "a failed import was reported as success"
fi

# --- a failed check fails the deploy without stopping anything ----------------
status=$(run_case 1 0)
if [ "$status" = 1 ] && ! grep -q 'stop\|--surreal' "$WORK_DIR/calls"; then
  pass "a failed emptiness check stops the deploy before touching the backend"
else
  fail "check failure: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- docker never sees the rendered telemetry variables -----------------------
run_case 0 0 > /dev/null
if [ -s "$WORK_DIR/otel" ]; then
  fail "docker saw OTEL variables: $(cut -d= -f1 "$WORK_DIR/otel" | sort -u | tr '\n' ' ')"
else
  pass "docker never sees the OTEL_* variables"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all migrate-from-surreal.sh tests passed"
