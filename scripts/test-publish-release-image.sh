#!/bin/sh
# Tests for scripts/publish-release-image.sh (card #461).
#
# Plain POSIX sh with no harness dependency, wired into the Dockerfile like the
# other script tests. A fake `docker` first on PATH stands in for the
# registry: `manifest inspect` succeeds only for refs listed in
# $WORK_DIR/registry, and every call is recorded in order.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
UNDER_TEST="$SCRIPT_DIR/publish-release-image.sh"

WORK_DIR="$(mktemp -d)"
# shellcheck disable=SC2064  # expand WORK_DIR now, not at trap time
trap "rm -rf '$WORK_DIR'" EXIT

mkdir -p "$WORK_DIR/bin"
cat > "$WORK_DIR/bin/docker" <<EOF
#!/bin/sh
echo "\$*" >> "$WORK_DIR/docker-calls"
if [ "\$1 \$2" = "manifest inspect" ]; then
    grep -qx "\$3" "$WORK_DIR/registry"
fi
EOF
chmod +x "$WORK_DIR/bin/docker"

SHA=0123456789abcdef0123456789abcdef01234567
REPO=registry.example/bored

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

# run <registry refs...> — fresh registry and call log, then the script for
# version 1.69.341.
run() {
    : > "$WORK_DIR/registry"
    : > "$WORK_DIR/docker-calls"
    for ref in "$@"; do
        echo "$ref" >> "$WORK_DIR/registry"
    done
    PATH="$WORK_DIR/bin:$PATH" IMAGE_REPO="$REPO" \
        sh "$UNDER_TEST" 1.69.341 "$SHA" > "$WORK_DIR/out" 2> "$WORK_DIR/err"
}

# --- new version: pushed, then aliased as the commit's latest green build ----
expected_calls="manifest inspect $REPO:1.69.341
push $REPO:1.69.341
tag $REPO:1.69.341 $REPO:commit-$SHA
push $REPO:commit-$SHA"
if run "$REPO:1.69.300" && [ "$(cat "$WORK_DIR/docker-calls")" = "$expected_calls" ]; then
    pass "new version: version and commit tags pushed, in order"
else
    fail "new version: docker calls were: $(cat "$WORK_DIR/docker-calls") / $(cat "$WORK_DIR/err")"
fi

# --- version already in the registry: refused, nothing pushed ----------------
# An earlier build under a repeated pipeline number must not be overwritten,
# and the commit alias must not move to an image that was never pushed.
if run "$REPO:1.69.341"; then
    fail "existing version: expected a refusal"
elif grep -q "already exists" "$WORK_DIR/err" \
    && ! grep -q "^push\|^tag" "$WORK_DIR/docker-calls"; then
    pass "existing version: refused before any push or tag"
else
    fail "existing version: docker calls: $(cat "$WORK_DIR/docker-calls") / $(cat "$WORK_DIR/err")"
fi

# --- missing arguments: refused -------------------------------------------
if PATH="$WORK_DIR/bin:$PATH" sh "$UNDER_TEST" 1.69.341 > /dev/null 2>&1; then
    fail "missing sha: expected a refusal"
else
    pass "missing sha: refused"
fi

if [ "$failures" -ne 0 ]; then
    echo "$failures test(s) failed" >&2
    exit 1
fi
echo "all publish-release-image tests passed"
