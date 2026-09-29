#!/bin/sh
# Tests for scripts/compose-up-with-otel.sh (card #415).
#
# Plain POSIX sh with no harness dependency, like test-docker-git-credential.sh,
# and wired into the Dockerfile the same way, so the image build runs it.
#
# The script under test runs `docker compose "$@"`. Each case puts a fake
# `docker` first on PATH that records what the real one would have seen: its
# own environment's OTEL_* variables (there must be none) and the contents of
# the env file compose reads (there must be all of them). The script is copied
# into a scratch tree so its `../deploy/otel.env` lands there, not in the repo.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

WORK_DIR="$(mktemp -d)"
# shellcheck disable=SC2064  # expand WORK_DIR now, not at trap time
trap "rm -rf '$WORK_DIR'" EXIT

mkdir -p "$WORK_DIR/tree/scripts" "$WORK_DIR/tree/deploy" "$WORK_DIR/bin"
cp "$SCRIPT_DIR/compose-up-with-otel.sh" "$WORK_DIR/tree/scripts/"
UNDER_TEST="$WORK_DIR/tree/scripts/compose-up-with-otel.sh"
ENV_FILE="$WORK_DIR/tree/deploy/otel.env"

# The fake docker: record its OTEL_* environment, the env file, and its args.
cat > "$WORK_DIR/bin/docker" <<EOF
#!/bin/sh
env | grep '^OTEL_' > "$WORK_DIR/docker-env" || true
cp "$ENV_FILE" "$WORK_DIR/seen-env-file" 2>/dev/null || echo MISSING > "$WORK_DIR/seen-env-file"
echo "\$*" > "$WORK_DIR/docker-args"
EOF
chmod +x "$WORK_DIR/bin/docker"

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

# --- case 1: the layer rendered two variables --------------------------------
out="$WORK_DIR/case1.out"
PATH="$WORK_DIR/bin:$PATH" \
    OTEL_SERVICE_NAME=bored \
    OTEL_RESOURCE_ATTRIBUTES='deployment.environment.name=dev,telemetry_source=otlp' \
    sh "$UNDER_TEST" -p proj up -d > "$out"

if [ -s "$WORK_DIR/docker-env" ]; then
    fail "docker itself saw OTEL_* variables: $(cat "$WORK_DIR/docker-env")"
else
    pass "docker itself sees no OTEL_* variable"
fi
if grep -qx 'OTEL_SERVICE_NAME=bored' "$WORK_DIR/seen-env-file" \
    && grep -qx 'OTEL_RESOURCE_ATTRIBUTES=deployment.environment.name=dev,telemetry_source=otlp' "$WORK_DIR/seen-env-file"; then
    pass "the env file compose reads carries every variable, verbatim"
else
    fail "env file contents: $(cat "$WORK_DIR/seen-env-file")"
fi
if [ "$(cat "$WORK_DIR/docker-args")" = "compose -p proj up -d" ]; then
    pass "arguments are passed to docker compose unchanged"
else
    fail "docker args: $(cat "$WORK_DIR/docker-args")"
fi
if [ -e "$ENV_FILE" ]; then
    fail "the env file was left behind"
else
    pass "the env file is deleted afterwards"
fi
if grep -q 'OTEL_SERVICE_NAME' "$out" && ! grep -q 'telemetry_source=otlp' "$out"; then
    pass "the script names the variables and prints no value"
else
    fail "script output: $(cat "$out")"
fi

# --- case 2: no layer rendered (telemetry off) --------------------------------
out="$WORK_DIR/case2.out"
env -u OTEL_SERVICE_NAME -u OTEL_RESOURCE_ATTRIBUTES PATH="$WORK_DIR/bin:$PATH" \
    sh "$UNDER_TEST" up -d > "$out"
if [ ! -s "$WORK_DIR/seen-env-file" ] && grep -q 'none' "$out"; then
    pass "with no OTEL_* variable the env file is empty and the script says none"
else
    fail "off case: file '$(cat "$WORK_DIR/seen-env-file")', output '$(cat "$out")'"
fi

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "all compose-up-with-otel tests passed"
