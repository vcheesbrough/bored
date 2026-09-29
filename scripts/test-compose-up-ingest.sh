#!/bin/sh
# Tests for scripts/compose-up-ingest.sh (card #416).
#
# Plain POSIX sh, wired into the Dockerfile beside test-compose-up-with-otel.sh
# so every image build runs it. A fake `docker` first on PATH records what the
# real one would have seen: its own environment (no collector variable may be
# in it) and the env file compose reads (exactly the allowlisted variables).
# The script is copied into a scratch tree so `../deploy/ingest.env` lands
# there, not in the repo.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

WORK_DIR="$(mktemp -d)"
# shellcheck disable=SC2064  # expand WORK_DIR now, not at trap time
trap "rm -rf '$WORK_DIR'" EXIT

mkdir -p "$WORK_DIR/tree/scripts" "$WORK_DIR/tree/deploy" "$WORK_DIR/bin"
cp "$SCRIPT_DIR/compose-up-ingest.sh" "$WORK_DIR/tree/scripts/"
UNDER_TEST="$WORK_DIR/tree/scripts/compose-up-ingest.sh"
ENV_FILE="$WORK_DIR/tree/deploy/ingest.env"

cat > "$WORK_DIR/bin/docker" <<EOF
#!/bin/sh
env | grep -E '^(OIDC_|OTEL_|ALLOWED_|CLAIM_|CLIENT_|LOG_OUTPUT)' > "$WORK_DIR/docker-env" || true
cp "$ENV_FILE" "$WORK_DIR/seen-env-file" 2>/dev/null || echo MISSING > "$WORK_DIR/seen-env-file"
echo "\$*" > "$WORK_DIR/docker-args"
EOF
chmod +x "$WORK_DIR/bin/docker"

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

# --- case 1: a rendered layer, plus unrelated variables in the step ----------
out="$WORK_DIR/case1.out"
PATH="$WORK_DIR/bin:$PATH" \
    OIDC_ISSUER_URL='https://auth.example/application/o/bored-dev/' \
    OIDC_AUDIENCE=bored-browser-dev \
    ALLOWED_SERVICE_NAMES=bored-spa \
    CLAIM_ATTRIBUTES='sub=user.id,preferred_username=user.name' \
    OTEL_EXPORTER_OTLP_ENDPOINT=http://monitor-alloy:4318 \
    OTEL_RESOURCE_ATTRIBUTES='deployment.environment.name=dev,telemetry_source=otlp' \
    REGISTRY_PASSWORD=not-for-the-ingest \
    OTEL_TRACES_SAMPLER=not-allowlisted \
    sh "$UNDER_TEST" -p proj up -d > "$out"

if [ -s "$WORK_DIR/docker-env" ]; then
    fail "docker itself saw collector variables: $(cut -d= -f1 "$WORK_DIR/docker-env" | tr '\n' ' ')"
else
    pass "docker itself sees none of the collector's variables"
fi
if grep -qx 'OIDC_ISSUER_URL=https://auth.example/application/o/bored-dev/' "$WORK_DIR/seen-env-file" \
    && grep -qx 'CLAIM_ATTRIBUTES=sub=user.id,preferred_username=user.name' "$WORK_DIR/seen-env-file" \
    && grep -qx 'OTEL_EXPORTER_OTLP_ENDPOINT=http://monitor-alloy:4318' "$WORK_DIR/seen-env-file"; then
    pass "the env file carries the allowlisted variables verbatim"
else
    fail "env file names: $(cut -d= -f1 "$WORK_DIR/seen-env-file" | tr '\n' ' ')"
fi
if grep -q 'REGISTRY_PASSWORD\|OTEL_TRACES_SAMPLER' "$WORK_DIR/seen-env-file"; then
    fail "a variable outside the allowlist reached the env file"
else
    pass "nothing outside the allowlist reaches the container"
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
if grep -q 'OIDC_AUDIENCE' "$out" && ! grep -q 'bored-browser-dev' "$out"; then
    pass "the script names the variables and prints no value"
else
    fail "script output: $(cat "$out")"
fi

# --- case 2: nothing rendered — refuse rather than start a broken ingest -----
rm -f "$WORK_DIR/docker-args"
if env -i PATH="$WORK_DIR/bin:/usr/bin:/bin" sh "$UNDER_TEST" up -d > /dev/null 2>&1; then
    fail "an empty render still ran compose"
elif [ -e "$WORK_DIR/docker-args" ]; then
    fail "docker was invoked with no configuration"
else
    pass "with no configuration the script fails without calling docker"
fi

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "all compose-up-ingest tests passed"
