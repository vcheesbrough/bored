#!/bin/sh
# Run `docker compose "$@"` with the telemetry variables handed to the
# *container* and hidden from docker itself (card #415).
#
# Called by the deploy steps as the command `sovereign-config render
# /bored/devops/<env>/otel` runs, so on entry the OTEL_* variables of that
# layer are in this process's environment. Two things must not happen to them:
#
# 1. The docker CLI and compose plugin must not see them. Both are
#    OpenTelemetry-instrumented: with OTEL_EXPORTER_OTLP_ENDPOINT and
#    OTEL_SERVICE_NAME set they export their *own* spans under bored's name,
#    and the CLI rewrites OTEL_RESOURCE_ATTRIBUTES for the plugins it starts —
#    which is how the first deploy of this branch handed the backend a
#    resource-attribute list without `deployment.environment.name`.
# 2. They must not be written into the compose file, which holds no value.
#
# So they are written to `deploy/otel.env` (read by compose's `env_file:`,
# `required: false`, so a deploy without the layer, e2e and local runs get
# nothing), removed from this environment, and the file is deleted again once
# compose has read it. The file can hold a credential if the layer ever gains
# OTEL_EXPORTER_OTLP_HEADERS, hence the umask; nothing here prints a value.
set -eu

here=$(dirname "$0")
env_file="$here/../deploy/otel.env"

umask 077
# `env` prints NAME=VALUE per line; keep the OTEL_ family only. `|| true`: grep
# exits 1 when nothing matches, which is the telemetry-off case, not an error.
env | grep '^OTEL_' > "$env_file" || true
trap 'rm -f "$env_file"' EXIT

# Say which variables were passed — names only, never values.
names=$(sed -n 's/^\(OTEL_[A-Z0-9_]*\)=.*/\1/p' "$env_file")
echo "telemetry variables for the container: ${names:-none}" | tr '\n' ' '
echo

for name in $names; do
  unset "$name"
done

docker compose "$@"
