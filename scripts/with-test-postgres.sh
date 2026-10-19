#!/bin/sh
# Run a command with a throwaway Postgres for the backend's tests (card #471).
#
# Usage: with-test-postgres.sh <command> [args...]
#   CI:    with-test-postgres.sh docker build --secret id=database_url,env=DATABASE_URL ...
#   local: with-test-postgres.sh cargo test -p backend
#
# Starts a sibling `postgres:16-alpine` container on the same docker daemon,
# waits until it accepts connections, exports DATABASE_URL pointing at it, runs
# the command, and removes the container however the command ends. The tests
# create one database per test on it (`tests/db_support.rs`), so nothing needs
# cleaning between runs — the container is the cleanup. The same pattern as
# v-note's scripts/rust-ci-test.sh.
#
# The URL uses the container's bridge IP, which both the host and a BuildKit
# `RUN` step can reach. The password is a fixed test value: the container is
# unpublished and lives only for this command.

set -eu

[ "$#" -gt 0 ] || { echo "usage: $0 <command> [args...]" >&2; exit 2; }

# Same image, same digest as production (deploy/docker-compose.yml).
IMAGE=postgres:16-alpine@sha256:16bc17c64a573ef34162af9298258d1aec548232985b33ed7b1eac33ba35c229
PASSWORD=bored-ci-test
NAME="bored-test-postgres-$$-$(date +%s)"

# A cancelled CI run never reaches its trap; reap leftovers older than an hour
# (by label, so no other container is touched) before starting a new one.
for stale in $(docker ps -aq --filter label=bored.test-postgres=1 \
                 --filter "until=1h" 2>/dev/null || true); do
  docker rm -f "$stale" > /dev/null 2>&1 || true
done

# `fsync=off` and friends: durability is worthless for a throwaway test
# database, and they make the suite's many CREATE DATABASEs fast. The
# connection limit covers a fully parallel suite (a few connections per test).
docker run -d --rm --name "$NAME" --label bored.test-postgres=1 \
  -e POSTGRES_PASSWORD="$PASSWORD" -e POSTGRES_USER=bored -e POSTGRES_DB=bored \
  "$IMAGE" -c fsync=off -c synchronous_commit=off -c full_page_writes=off \
  -c max_connections=400 > /dev/null
# shellcheck disable=SC2064  # expand NAME now, not at trap time
trap "docker rm -f '$NAME' > /dev/null 2>&1 || true" EXIT INT TERM

ready=0
for _ in $(seq 1 60); do
  if docker exec "$NAME" pg_isready -h 127.0.0.1 -U bored -d bored > /dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
[ "$ready" = 1 ] || { echo "ERROR: test postgres did not become ready" >&2; exit 1; }

IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$NAME")
[ -n "$IP" ] || { echo "ERROR: test postgres has no bridge IP" >&2; exit 1; }
export DATABASE_URL="postgres://bored:$PASSWORD@$IP:5432/bored"
echo "test postgres ready at $IP"

# Not `exec`: the trap must still run to remove the container.
"$@"
