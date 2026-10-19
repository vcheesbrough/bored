#!/bin/sh
# The app half of a deploy (card #471): the one-off SurrealDB import check,
# then the compose up. Run under `sovereign-config render … --` so both see the
# rendered telemetry and compose layers.
#
# Usage: deploy-app.sh <docker compose project args...>
#   e.g. deploy-app.sh -p bored-dev -f deploy/docker-compose.yml
#
# A script rather than an inline `sh -c '…'` in .woodpecker/build.yml: a
# multi-line single-quoted block there broke the shell Woodpecker generates
# for the step (deploy 385: "unterminated quoted string").

set -eu

here=$(dirname "$0")
"$here/migrate-from-surreal.sh" "$@"
exec "$here/compose-up-with-otel.sh" "$@" up -d --pull always --wait --wait-timeout 120
