#!/bin/sh
# Tests for scripts/compute-release-tag.sh (card #461).
#
# Plain POSIX sh with no harness dependency, wired into the Dockerfile as its
# own RUN layer like the other script tests, so every image build checks it.
# Expected tags are literal strings, not recomputed from the fixture.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
UNDER_TEST="$SCRIPT_DIR/compute-release-tag.sh"

WORK_DIR="$(mktemp -d)"
# shellcheck disable=SC2064  # expand WORK_DIR now, not at trap time
trap "rm -rf '$WORK_DIR'" EXIT

failures=0

pass() {
    echo "ok   - $1"
}

fail() {
    echo "FAIL - $1" >&2
    failures=$((failures + 1))
}

# A manifest shaped like the real one: a member-ish table *before* the
# workspace one and a dependency with its own version after it, so only the
# [workspace.package] version may be read.
cat >"$WORK_DIR/Cargo.toml" <<'EOF'
[workspace]
members = ["shared"]

[package]
version = "9.9.9"

[workspace.package]
version = "1.69.0"
edition = "2024"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
EOF

# run <event> <number> <parent> [cargo_toml] — stdout to $WORK_DIR/out,
# stderr to $WORK_DIR/err; returns the script's exit status.
run() {
    CI_PIPELINE_EVENT="$1" CI_PIPELINE_NUMBER="$2" CI_PIPELINE_PARENT="$3" \
        sh "$UNDER_TEST" "${4:-$WORK_DIR/Cargo.toml}" >"$WORK_DIR/out" 2>"$WORK_DIR/err"
}

# expect_tag <name> <expected> <event> <number> <parent>
expect_tag() {
    name="$1"
    expected="$2"
    shift 2
    if run "$@" && [ "$(cat "$WORK_DIR/out")" = "$expected" ]; then
        pass "$name"
    else
        fail "$name: expected '$expected', got '$(cat "$WORK_DIR/out")' / $(cat "$WORK_DIR/err")"
    fi
}

# expect_refusal <name> <stderr-substring> <event> <number> <parent> [cargo_toml]
expect_refusal() {
    name="$1"
    needle="$2"
    shift 2
    if run "$@"; then
        fail "$name: expected a refusal, got '$(cat "$WORK_DIR/out")'"
    elif grep -q "$needle" "$WORK_DIR/err"; then
        pass "$name"
    else
        fail "$name: refused, but stderr lacks '$needle': $(cat "$WORK_DIR/err")"
    fi
}

expect_tag "push: patch is this pipeline's number" "1.69.339" push 339 0
expect_tag "manual: patch is this pipeline's number" "1.69.340" manual 340 0
expect_tag "deployment: patch is the parent push pipeline's number" "1.69.339" deployment 341 339

expect_refusal "deployment without a parent is refused" "no parent pipeline" deployment 341 0
expect_refusal "deployment with an empty parent is refused" "no parent pipeline" deployment 341 ""
expect_refusal "push with a non-numeric number is refused" "not a pipeline number" push "12a" 0
expect_refusal "push with a leading-zero number is refused" "not a pipeline number" push "012" 0
expect_refusal "unknown event is refused" "unsupported CI_PIPELINE_EVENT" pull_request 5 0

printf '[workspace.package]\nedition = "2024"\n' >"$WORK_DIR/no-version.toml"
expect_refusal "manifest without a workspace version is refused" "no MAJOR.MINOR.PATCH" push 5 0 "$WORK_DIR/no-version.toml"

if [ "$failures" -ne 0 ]; then
    echo "$failures test(s) failed" >&2
    exit 1
fi
echo "all compute-release-tag tests passed"
