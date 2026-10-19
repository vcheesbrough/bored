#!/bin/sh
# The guarded, one-off SurrealDB -> Postgres import of a deploy (card #471).
#
# Usage: BORED_CONTAINER_NAME=<backend container> DB_VOLUME=<old store volume> \
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

# Nothing in here reads input. `docker compose run` would otherwise attach
# stdin, which in CI is the rest of the step's own script (see deploy-app.sh).
# This line is the fix; the `-T` on the runs below only drops the pseudo-TTY
# (there is no terminal in CI) and does not, by itself, detach stdin.
exec < /dev/null

: "${BORED_CONTAINER_NAME:?BORED_CONTAINER_NAME is required}"
: "${DB_VOLUME:?DB_VOLUME is required}"

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
docker compose "$@" --profile surreal-migration run --rm --no-deps -T surreal-migrate --check-empty \
  || status=$?
case "$status" in
  3)
    # Data in Postgres while the deployed backend is still the SurrealDB build
    # means the environment was rolled back after its cut-over: anything
    # written since lives only in SurrealDB, and deploying on would silently
    # drop it. Stop, and leave the reconciliation to a person (README §
    # Database, "Rolling back").
    #
    # Only a *running* SurrealDB build counts. A stopped one is what a
    # cut-over leaves behind when its import committed but the `compose up`
    # after it failed (a registry hiccup, say): retrying must carry on, and
    # stopping the old backend is also how a person chooses Postgres after a
    # rollback (README § Database).
    if docker inspect "$BORED_CONTAINER_NAME" > /dev/null 2>&1; then
      state=$(docker inspect -f '{{.State.Running}}|{{range .Mounts}}{{.Name}} {{end}}' "$BORED_CONTAINER_NAME")
      case " ${state#*|} " in
        *" $DB_VOLUME "*) running=${state%%|*} ;;
        *) running=false ;;
      esac
      case "$running" in
        true)
          echo "ERROR: postgres already holds data, but $BORED_CONTAINER_NAME is a SurrealDB" >&2
          echo "       build (it mounts $DB_VOLUME): the environment was rolled back after" >&2
          echo "       its move to postgres. Deploying would drop what was written to" >&2
          echo "       SurrealDB since. Reconcile by hand (README § Database) first." >&2
          exit 1
          ;;
      esac
    fi
    echo "postgres already holds bored data; no import needed"
    exit 0
    ;;
  0)
    ;;
  *)
    echo "ERROR: the import's emptiness check failed (exit $status)" >&2
    exit "$status"
    ;;
esac

# Import only at the cut-over: while the deployed backend is still the
# SurrealDB build, i.e. still mounts the old store's volume. Once an
# environment runs on Postgres, an empty Postgres means its data was lost or
# is mid-restore, and re-importing the retired store would quietly bring back
# months-old data and report success. Refuse instead: restore the pg_dump
# (mini-config RECOVERY.md step 11b). No backend container at all is a new
# host or environment, where the old volume is empty (a restore skips its
# tar) and the import is a no-op.
if docker inspect "$BORED_CONTAINER_NAME" > /dev/null 2>&1; then
  mounts=$(docker inspect -f '{{range .Mounts}}{{.Name}} {{end}}' "$BORED_CONTAINER_NAME")
  case " $mounts " in
    *" $DB_VOLUME "*) ;;
    *)
      # Nothing to re-import when the old volume holds no store (a new
      # environment that has run on Postgres from the start, before anyone
      # has written to it): carry on with an empty database.
      #
      # `test -e` answers 0 (a store is there) or 1 (none); anything else is
      # docker itself failing (a pull error, a missing volume), which must stop
      # the deploy rather than read as "no store" and fail open.
      store=0
      docker run --rm -v "$DB_VOLUME:/surreal:ro" \
          alpine:3.21@sha256:f27cad9117495d32d067133afff942cb2dc745dfe9163e949f6bfe8a6a245339 \
          test -e /surreal/bored.db || store=$?
      case "$store" in
        1)
          echo "no SurrealDB store in $DB_VOLUME; nothing to import"
          exit 0
          ;;
        0) ;;
        *)
          echo "ERROR: could not check $DB_VOLUME for a SurrealDB store (exit $store)" >&2
          exit "$store"
          ;;
      esac
      echo "ERROR: postgres is empty, but $BORED_CONTAINER_NAME already runs on postgres" >&2
      echo "       (it does not mount $DB_VOLUME). Refusing to re-import the retired" >&2
      echo "       SurrealDB store; restore the latest pg_dump instead (mini-config" >&2
      echo "       RECOVERY.md step 11b), then redeploy." >&2
      exit 1
      ;;
  esac
fi
echo "postgres is empty; importing the SurrealDB store"

# The old backend may already be gone (a first deploy, or a retry after a
# failed import); that is fine.
docker stop -t 15 "$BORED_CONTAINER_NAME" || echo "no running $BORED_CONTAINER_NAME to stop"

status=0
docker compose "$@" --profile surreal-migration run --rm --no-deps -T surreal-migrate \
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
