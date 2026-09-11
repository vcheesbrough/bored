#!/bin/sh
# Run a long build command, emitting a heartbeat while it works.
#
# Cargo prints "Compiling <crate>" once and then says nothing until the crate is
# done. For the frontend that gap is minutes, and in a CI log a silent step is
# indistinguishable from a hung one — you cannot tell progress from a deadlock,
# and there is no way to judge how much longer to wait.
#
# This wrapper streams the command's own output unchanged and interleaves an
# elapsed-time heartbeat, so every step reports that it is alive and how long it
# has been going.
#
# Usage: with-progress.sh <label> <command...>
set -eu

label=$1
shift

start=$(date +%s)
echo "[$label] starting: $*"

# Heartbeat in the background. Killed on exit so it can never outlive the step.
(
  while true; do
    sleep 15
    now=$(date +%s)
    echo "[$label] still running: $((now - start))s elapsed"
  done
) &
heartbeat=$!
# shellcheck disable=SC2064  # $heartbeat must expand now, not at trap time.
trap "kill $heartbeat 2>/dev/null || true" EXIT INT TERM

set +e
"$@"
rc=$?
set -e

kill "$heartbeat" 2>/dev/null || true
end=$(date +%s)
echo "[$label] finished in $((end - start))s (exit $rc)"
exit $rc
