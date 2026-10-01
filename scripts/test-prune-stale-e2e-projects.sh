#!/bin/sh
# Tests for scripts/prune-stale-e2e-projects.sh (card #461).
#
# Plain POSIX sh with no harness dependency, wired into the Dockerfile like the
# other script tests. A fake `docker` first on PATH answers `ps -a` from
# $WORK_DIR/ps ("<project>|<RunningFor>" per line), returns one id per
# project for the label-filtered listings, and records every removal.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
UNDER_TEST="$SCRIPT_DIR/prune-stale-e2e-projects.sh"

WORK_DIR="$(mktemp -d)"
# shellcheck disable=SC2064  # expand WORK_DIR now, not at trap time
trap "rm -rf '$WORK_DIR'" EXIT

mkdir -p "$WORK_DIR/bin"
cat > "$WORK_DIR/bin/docker" <<'EOF'
#!/bin/sh
# Label-filtered listings: "--filter label=com.docker.compose.project=<p>" → an
# id naming the kind and project.
project_of() {
    for a in "$@"; do
        case "$a" in label=com.docker.compose.project=*) echo "${a#label=com.docker.compose.project=}" ;; esac
    done
}
case "$1 $2" in
    "ps -a")
        if [ "$3" = "--format" ] || [ "$4" = "label=com.docker.compose.project" ]; then
            cat "$WORK_DIR/ps"
        else
            echo "ctr-$(project_of "$@")"
        fi
        ;;
    "ps -aq") echo "ctr-$(project_of "$@")" ;;
    "network ls") echo "net-$(project_of "$@")" ;;
    "volume ls") echo "vol-$(project_of "$@")" ;;
    "rm -f" | "network rm" | "volume rm") echo "$*" >> "$WORK_DIR/removed" ;;
    *) echo "unexpected: $*" >&2; exit 2 ;;
esac
EOF
sed -i "s|\$WORK_DIR|$WORK_DIR|g" "$WORK_DIR/bin/docker"
chmod +x "$WORK_DIR/bin/docker"

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

# run <ps lines...> — fresh listing and removal log, current project 350.
run() {
    : > "$WORK_DIR/ps"
    : > "$WORK_DIR/removed"
    for line in "$@"; do
        echo "$line" >> "$WORK_DIR/ps"
    done
    PATH="$WORK_DIR/bin:$PATH" sh "$UNDER_TEST" bored-e2e-350 > "$WORK_DIR/out" 2> "$WORK_DIR/err"
}

run \
    "bored-e2e-301|3 hours ago" \
    "bored-e2e-301|3 hours ago" \
    "bored-e2e-349|4 minutes ago" \
    "bored-e2e-348|About an hour ago" \
    "bored-e2e-350|2 days ago" \
    "bored-e2e-302|2 days ago" \
    "bored-e2e-302|10 minutes ago" \
    "bored-dev|2 weeks ago" \
    "e2e|5 days ago" \
    "bored-e2e-old|2 months ago"

expected_removed="rm -f ctr-bored-e2e-301
network rm net-bored-e2e-301
volume rm vol-bored-e2e-301"
if [ "$(cat "$WORK_DIR/removed")" = "$expected_removed" ]; then
    pass "only the hours-old bored-e2e-<n> project, not the current one, is removed"
else
    fail "removed: $(cat "$WORK_DIR/removed") / $(cat "$WORK_DIR/err")"
fi

# Spelled out, so a regression names the case it broke.
kept_ok=true
for keep in bored-e2e-349 bored-e2e-348 bored-e2e-350 bored-e2e-302 bored-dev "e2e" bored-e2e-old; do
    if grep -q -- "-$keep\$" "$WORK_DIR/removed"; then
        fail "kept project $keep was removed"
        kept_ok=false
    fi
done
if $kept_ok; then
    pass "minutes-old, 'About an hour', current, partly-young and non-e2e projects are kept"
fi

run
if [ ! -s "$WORK_DIR/removed" ]; then
    pass "nothing listed: nothing removed"
else
    fail "nothing listed: removed $(cat "$WORK_DIR/removed")"
fi

if [ "$failures" -ne 0 ]; then
    echo "$failures test(s) failed" >&2
    exit 1
fi
echo "all prune-stale-e2e-projects tests passed"
