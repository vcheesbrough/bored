#!/bin/sh
# Guards a rule nothing else in CI can see (card #473): every service Alloy is
# meant to scrape (label observability.metrics.scrape=true) must have
# `proxy-backend` as its FIRST network by name.
#
# Alloy's Docker discovery, like Prometheus' docker_sd with
# match_first_network, sees a multi-network container only through its first
# network sorted by name, and mini-config's app-metrics scrape keeps
# proxy-backend targets alone. A network that sorts earlier silently drops the
# service's metrics — which is what happened to the SQL console until its
# private network was named `storage-…` (deploy/docker-compose.yml).
#
# Usage: test-metrics-network-order.sh [compose file]  (needs docker compose, jq)

set -eu

file="${1:-deploy/docker-compose.yml}"

# Dummy values for every required variable, and every profile on, so the whole
# file resolves; `--format json` gives the final network names.
config=$(
  BORED_IMAGE_TAG=x BORED_CONTAINER_NAME=bored-dev APP_ENV=dev BORED_HOST=h \
  DB_VOLUME=a PG_VOLUME=b POSTGRES_PASSWORD=p SOVEREIGN_CONFIG_ACCESS_URL_FILE=u \
  PGWEB_DB_PASSWORD=p PGWEB_AUTH_USER=u PGWEB_AUTH_PASS=p PGWEB_AUTH_B64=b \
  COMPOSE_PROFILES='*' \
    docker compose -f "$file" config --format json 2>/dev/null
)

# For each scraped service: its networks' real names, sorted; the first must be
# proxy-backend. Prints one line per offending service.
bad=$(printf '%s' "$config" | jq -r '
  . as $root
  | .services | to_entries[]
  | select((.value.labels // {}) | to_entries | any(.value == "true" and .key == "observability.metrics.scrape"))
  | .key as $svc
  | [(.value.networks // {}) | keys[] | ($root.networks[.].name // .)] | sort
  | select(.[0] != "proxy-backend")
  | "\($svc): first network by name is \(.[0]) (all: \(join(", ")))"')
scraped=$(printf '%s' "$config" | jq '[.services[] | select((.labels // {})["observability.metrics.scrape"] == "true")] | length')

if [ "$scraped" -eq 0 ]; then
  echo "FAIL - no scraped service found in $file; the check is checking nothing" >&2
  exit 1
fi
if [ -n "$bad" ]; then
  echo "FAIL - a scraped service would be invisible to Alloy:" >&2
  echo "$bad" >&2
  exit 1
fi
echo "ok   - $scraped scraped service(s) all see proxy-backend first"
