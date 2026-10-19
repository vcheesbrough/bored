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
  "inspect -f"*) cat "$WORK_DIR/mounts" ;;
  "run --rm -v"*)
    case "\$(cat "$WORK_DIR/store")" in yes) exit 0 ;; no) exit 1 ;; *) exit 125 ;; esac ;;
  inspect*) [ -s "$WORK_DIR/mounts" ] || exit 1 ;;
  *--check-empty*) exit "\$(cat "$WORK_DIR/check-status")" ;;
  *--surreal*) exit "\$(cat "$WORK_DIR/import-status")" ;;
esac
exit 0
FAKE
chmod +x "$WORK_DIR/bin/docker"

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

# Run the script with the given check and import statuses, the backend
# container's mounts as $3 (default: the SurrealDB build, mounting the old
# store; "none" for no container at all), and whether the old volume holds a
# store as $4 (default yes); echo its status.
run_case() {
  : > "$WORK_DIR/calls"
  : > "$WORK_DIR/otel"
  echo "$1" > "$WORK_DIR/check-status"
  echo "$2" > "$WORK_DIR/import-status"
  case "${3:-old}" in
    old) echo "bored-dev-db " > "$WORK_DIR/mounts" ;;
    none) : > "$WORK_DIR/mounts" ;;
    *) echo "$3 " > "$WORK_DIR/mounts" ;;
  esac
  echo "${4:-yes}" > "$WORK_DIR/store"
  status=0
  PATH="$WORK_DIR/bin:$PATH" BORED_CONTAINER_NAME=bored-dev DB_VOLUME=bored-dev-db \
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

# --- a failed import fails the deploy, with the old backend back up -----------
status=$(run_case 0 1)
if [ "$status" != 0 ]; then
  pass "a failed import fails the script"
else
  fail "a failed import was reported as success"
fi
import_line=$(grep -n -- '--surreal /surreal/bored.db' "$WORK_DIR/calls" | cut -d: -f1)
start_line=$(grep -n 'start bored-dev' "$WORK_DIR/calls" | cut -d: -f1)
if [ -n "$start_line" ] && [ "$start_line" -gt "$import_line" ]; then
  pass "a failed import restarts the old backend"
else
  fail "no restart after a failed import: $(cat "$WORK_DIR/calls")"
fi

# --- a successful import does not restart the old backend ---------------------
run_case 0 0 > /dev/null
if grep -q 'start bored-dev' "$WORK_DIR/calls"; then
  fail "the old backend was restarted after a successful import"
else
  pass "a successful import leaves the old backend stopped for compose up"
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

# --- an environment already on postgres never re-imports ----------------------
status=$(run_case 0 0 some-other-volume)
if [ "$status" = 1 ] && ! grep -q 'stop\|--surreal' "$WORK_DIR/calls"; then
  pass "an empty postgres behind a postgres-era backend refuses to re-import"
else
  fail "re-import guard: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- ... unless the old volume holds no store: nothing could be re-imported ---
status=$(run_case 0 0 some-other-volume no)
if [ "$status" = 0 ] && ! grep -q 'stop\|--surreal' "$WORK_DIR/calls"; then
  pass "a postgres-era environment with an empty old volume deploys on"
else
  fail "empty old volume: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- ... and a failed check of the old volume stops the deploy (fail closed) ---
status=$(run_case 0 0 some-other-volume broken)
if [ "$status" = 125 ] && ! grep -q 'stop\|--surreal' "$WORK_DIR/calls"; then
  pass "a docker failure checking the old volume fails the deploy"
else
  fail "volume check failure: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- no backend container at all (a new host) imports -------------------------
status=$(run_case 0 0 none)
if [ "$status" = 0 ] && grep -q -- '--surreal /surreal/bored.db' "$WORK_DIR/calls"; then
  pass "a new environment with no backend container runs the import"
else
  fail "new environment: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all migrate-from-surreal.sh tests passed"
