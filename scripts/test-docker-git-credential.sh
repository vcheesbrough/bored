#!/bin/sh
# Tests for scripts/docker-git-credential.sh.
#
# Plain POSIX sh with no harness dependency, so it runs in any of the build
# stages. Wired into the Dockerfile as its own RUN layer, which means the
# documented local repro (`docker build --secret id=github_token,... .`) covers
# it just as it covers rustfmt, clippy and the cargo tests.
#
# The script under test is *sourced*, and on the malformed-secret path it
# `return`s non-zero. Each case therefore runs in a subshell: the subshell both
# isolates the exported GIT_CONFIG_* vars between cases and gives `return` a
# caller whose exit status we can inspect.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
UNDER_TEST="$SCRIPT_DIR/docker-git-credential.sh"

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

# --- case 1: secret absent -------------------------------------------------
# Unchanged legacy behaviour: warn, succeed, export nothing. The build then
# fails later with a plain auth error, which is already legible.
absent_out="$WORK_DIR/absent.err"
if (
    GITHUB_TOKEN_FILE="$WORK_DIR/does-not-exist"
    export GITHUB_TOKEN_FILE
    # shellcheck disable=SC1090  # path is computed
    . "$UNDER_TEST" 2>"$absent_out"
    # An absent secret must not leave a rewrite behind.
    [ -z "${GIT_CONFIG_COUNT:-}" ]
); then
    if grep -q "WARNING" "$absent_out"; then
        pass "absent secret: warns and continues without exporting GIT_CONFIG_*"
    else
        fail "absent secret: expected a WARNING on stderr, got: $(cat "$absent_out")"
    fi
else
    fail "absent secret: expected success with no GIT_CONFIG_COUNT set"
fi

# --- case 2: empty secret --------------------------------------------------
# `-s` treats a zero-byte file as absent; same path as case 1.
: >"$WORK_DIR/empty"
empty_out="$WORK_DIR/empty.err"
if (
    GITHUB_TOKEN_FILE="$WORK_DIR/empty"
    export GITHUB_TOKEN_FILE
    # shellcheck disable=SC1090
    . "$UNDER_TEST" 2>"$empty_out"
    [ -z "${GIT_CONFIG_COUNT:-}" ]
); then
    pass "empty secret: treated as absent"
else
    fail "empty secret: expected the absent-secret path"
fi

# --- case 3: well-formed token ---------------------------------------------
# Not a real credential — any [A-Za-z0-9_] string exercises the same branch.
printf 'gho_0123456789abcdefTESTTOKEN' >"$WORK_DIR/good"
if (
    GITHUB_TOKEN_FILE="$WORK_DIR/good"
    export GITHUB_TOKEN_FILE
    # shellcheck disable=SC1090
    . "$UNDER_TEST"
    [ "${GIT_CONFIG_COUNT:-}" = "1" ] &&
        [ "${GIT_CONFIG_KEY_0:-}" = "url.https://x-access-token:gho_0123456789abcdefTESTTOKEN@github.com/.insteadOf" ] &&
        [ "${GIT_CONFIG_VALUE_0:-}" = "https://github.com/" ]
); then
    pass "well-formed token: exports the insteadOf rewrite"
else
    fail "well-formed token: GIT_CONFIG_* not exported as expected"
fi

# --- case 4: trailing newline ----------------------------------------------
# The regression guard that matters most: CI secret stores commonly append a
# newline. Command substitution strips it, so this must behave as case 3 and
# NOT be rejected as whitespace.
printf 'gho_0123456789abcdefTESTTOKEN\n' >"$WORK_DIR/trailing-newline"
if (
    GITHUB_TOKEN_FILE="$WORK_DIR/trailing-newline"
    export GITHUB_TOKEN_FILE
    # shellcheck disable=SC1090
    . "$UNDER_TEST"
    [ "${GIT_CONFIG_KEY_0:-}" = "url.https://x-access-token:gho_0123456789abcdefTESTTOKEN@github.com/.insteadOf" ]
); then
    pass "trailing newline: stripped, token still accepted"
else
    fail "trailing newline: token was rejected or mis-parsed"
fi

# --- case 5: captured error text -------------------------------------------
# The exact fault this guard exists for: `GITHUB_TOKEN=$(gh auth token)` on
# gh < 2.5.0 substitutes the command's error message.
printf 'unknown command "token" for "gh auth"' >"$WORK_DIR/error-text"
error_out="$WORK_DIR/error-text.err"
if (
    GITHUB_TOKEN_FILE="$WORK_DIR/error-text"
    export GITHUB_TOKEN_FILE
    # shellcheck disable=SC1090
    . "$UNDER_TEST" 2>"$error_out"
); then
    fail "captured error text: expected a non-zero return, got success"
else
    if grep -q "github_token" "$error_out"; then
        pass "captured error text: rejected with a message naming the credential"
    else
        fail "captured error text: message does not name github_token: $(cat "$error_out")"
    fi
fi

# --- case 6: whitespace inside an otherwise plausible value ----------------
printf 'not a token' >"$WORK_DIR/spaces"
spaces_out="$WORK_DIR/spaces.err"
if (
    GITHUB_TOKEN_FILE="$WORK_DIR/spaces"
    export GITHUB_TOKEN_FILE
    # shellcheck disable=SC1090
    . "$UNDER_TEST" 2>"$spaces_out"
); then
    fail "whitespace value: expected a non-zero return, got success"
else
    if grep -q "github_token" "$spaces_out"; then
        pass "whitespace value: rejected with a message naming the credential"
    else
        fail "whitespace value: message does not name github_token"
    fi
fi

# --- case 7: embedded newline ----------------------------------------------
# A leading/embedded newline survives command substitution (only *trailing*
# newlines are stripped), so it must be rejected.
printf 'gho_abc\ngho_def' >"$WORK_DIR/embedded-newline"
newline_out="$WORK_DIR/embedded-newline.err"
if (
    GITHUB_TOKEN_FILE="$WORK_DIR/embedded-newline"
    export GITHUB_TOKEN_FILE
    # shellcheck disable=SC1090
    . "$UNDER_TEST" 2>"$newline_out"
); then
    fail "embedded newline: expected a non-zero return, got success"
else
    pass "embedded newline: rejected"
fi

# --- summary ---------------------------------------------------------------
if [ "$failures" -eq 0 ]; then
    echo "docker-git-credential.sh: all checks passed"
else
    echo "docker-git-credential.sh: $failures check(s) failed" >&2
    exit 1
fi
