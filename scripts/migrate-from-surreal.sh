#!/bin/sh
# The guarded, one-off SurrealDB -> Postgres import of a deploy (card #471).
#
# Usage: BORED_CONTAINER_NAME=<old backend container> \
#            migrate-from-surreal.sh <docker compose project args...>
#   e.g. migrate-from-surreal.sh -p bored-dev -f deploy/docker-compose.yml
#
# Runs before the deploy's `compose up` of the new backend:
#   1. bring the Postgres service up and wait for it to be healthy;
#   2. ask the import (`--check-empty`) whether Postgres already holds data —
#      exit 3 means it does (already migrated, or a fresh environment that has
#      been used since), and the deploy carries on without touching anything;
#   3. otherwise stop the old backend, so the SurrealKV store is quiescent and
#      nothing is written after the copy is taken, and run the import.
# The import refuses a non-empty database on its own as well, and writes in one
# verified transaction, so a failure here leaves Postgres empty; the script
# then restarts the old backend and fails the deploy, and the next deploy tries
# again. The old volume is only ever mounted read-only.
#
# Delete this script with the import subcommand once both environments are
# migrated.

set -eu

: "${BORED_CONTAINER_NAME:?BORED_CONTAINER_NAME is required}"

# The docker CLI is OpenTelemetry-instrumented: with the rendered OTEL_*
# variables in its environment it would export its own spans under the
# backend's identity (see compose-up-with-otel.sh). None are needed here.
for name in $(env | sed -n 's/^\(OTEL_[A-Z0-9_]*\)=.*/\1/p'); do
  unset "$name"
done

docker compose "$@" up -d --wait --wait-timeout 120 bored-db

# `|| status=$?` keeps `set -e` from ending the script on the expected
# non-zero answer while still capturing it.
status=0
docker compose "$@" --profile surreal-migration run --rm --no-deps surreal-migrate --check-empty \
  || status=$?
case "$status" in
  3)
    echo "postgres already holds bored data; no import needed"
    exit 0
    ;;
  0)
    echo "postgres is empty; importing the SurrealDB store"
    ;;
  *)
    echo "ERROR: the import's emptiness check failed (exit $status)" >&2
    exit "$status"
    ;;
esac

# The old backend may already be gone (a first deploy, or a retry after a
# failed import); that is fine.
docker stop -t 15 "$BORED_CONTAINER_NAME" || echo "no running $BORED_CONTAINER_NAME to stop"

status=0
docker compose "$@" --profile surreal-migration run --rm --no-deps surreal-migrate \
  --surreal /surreal/bored.db || status=$?
if [ "$status" -ne 0 ]; then
  # The import rolled back, so Postgres is still empty and the old store is
  # untouched: put the old backend back in service before failing the deploy,
  # rather than leaving the environment down until someone intervenes. The
  # next deploy tries the import again.
  echo "ERROR: the import failed (exit $status); restarting $BORED_CONTAINER_NAME on SurrealDB" >&2
  docker start "$BORED_CONTAINER_NAME" || echo "ERROR: could not restart $BORED_CONTAINER_NAME" >&2
  exit "$status"
fi
