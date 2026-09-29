#!/bin/sh
# Run `docker compose "$@"` for the client-telemetry ingest with its
# configuration handed to the *container* and hidden from docker itself
# (card #416).
#
# Called by the deploy steps as the command `sovereign-config render
# /bored/devops/<env>/ingest` runs, so on entry that layer's variables are in
# this process's environment, alongside everything else the step has. The
# collector's configuration is exactly the variables named below — the
# `otlp-collector-oidc` settings bored uses (its docs/configuration.md) — so
# only those are taken:
#
# - written to `deploy/ingest.env`, read by deploy/otlp-ingest.yml's
#   `env_file:` and deleted again once compose has read it;
# - removed from this environment first, because the docker CLI is
#   OpenTelemetry-instrumented and would act on the OTEL_* ones itself (see
#   scripts/compose-up-with-otel.sh, which does the same for the app).
#
# Nothing here prints a value — only names.
set -eu

here=$(dirname "$0")
env_file="$here/../deploy/ingest.env"

# The collector variables this deploy may set. An allowlist rather than a
# prefix: the step's own environment holds unrelated OTEL_* and credentials
# that must never reach the ingest container.
pattern='^(OIDC_ISSUER_URL|OIDC_AUDIENCE|OIDC_DISCOVERY_RETRY|OIDC_JWKS_REFRESH|REQUIRED_SCOPE|REQUIRED_CLAIMS|CLAIM_ATTRIBUTES|CLIENT_RESOURCE_ATTRIBUTES|ALLOWED_SERVICE_NAMES|ALLOWED_METRIC_NAMES|ALLOWED_METRIC_ATTRIBUTE_KEYS|MAX_REQUEST_BODY_BYTES|CORS_ALLOWED_ORIGINS|LOG_LEVEL|LOG_OUTPUT|OTEL_SERVICE_NAME|OTEL_RESOURCE_ATTRIBUTES|OTEL_EXPORTER_OTLP_ENDPOINT|OTEL_EXPORTER_OTLP_PROTOCOL)='

umask 077
env | grep -E "$pattern" > "$env_file" || true
trap 'rm -f "$env_file"' EXIT

names=$(sed -n 's/^\([A-Z0-9_]*\)=.*/\1/p' "$env_file")
if [ -z "$names" ]; then
  echo "no ingest configuration rendered; not starting the ingest" >&2
  exit 1
fi
echo "ingest variables for the container: $names" | tr '\n' ' '
echo

for name in $names; do
  unset "$name"
done
# And any other OTEL_* the step happens to carry: the instrumented docker CLI
# must see none at all.
for name in $(env | sed -n 's/^\(OTEL_[A-Z0-9_]*\)=.*/\1/p'); do
  unset "$name"
done

docker compose "$@"
