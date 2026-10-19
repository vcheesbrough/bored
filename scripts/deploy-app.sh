#!/bin/sh
# The app half of a deploy (card #471): the one-off SurrealDB import check,
# then the compose up. Run under `sovereign-config render … --` so both see the
# rendered telemetry and compose layers.
#
# Usage: deploy-app.sh <docker compose project args...>
#   e.g. deploy-app.sh -p bored-dev -f deploy/docker-compose.yml
#
# A script rather than an inline `sh -c '…'` in .woodpecker/build.yml, which
# keeps the step's commands short and testable.

set -eu

here=$(dirname "$0")
# `</dev/null` on both: Woodpecker hands the step's script to the shell on
# stdin, and `docker compose run` (the import check) reads stdin by default —
# it ate the rest of the step script, which then failed with "unterminated
# quoted string" (deploys 385, 390, 391). Nothing here needs input.
"$here/migrate-from-surreal.sh" "$@" < /dev/null
exec "$here/compose-up-with-otel.sh" "$@" up -d --pull always --wait --wait-timeout 120 < /dev/null
