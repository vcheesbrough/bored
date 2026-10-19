#!/bin/sh
# The app half of a deploy: the one-off SurrealDB import check (card #471),
# the compose up of the backend and its database, then the read-only SQL
# console (card #473) when it is enabled. Run under `sovereign-config render
# … --` so every part sees the rendered telemetry and compose layers.
#
# Usage: deploy-app.sh <docker compose project args...>
#   e.g. deploy-app.sh -p bored-dev -f deploy/docker-compose.yml
#
# Environment, besides what deploy/docker-compose.yml interpolates:
#   COMPOSE_PROFILES                   naming `sqltool` enables the SQL console
# Required only when COMPOSE_PROFILES names `sqltool` (rendered from
# /bored/devops/<env>/compose):
#   PGWEB_DB_PASSWORD                  password for the read-only bored_pgweb role
#   PGWEB_AUTH_USER / PGWEB_AUTH_PASS  the console's basic-auth backstop; this
#                                      script derives PGWEB_AUTH_B64 from them for
#                                      the Traefik header that satisfies it
#
# A script rather than an inline `sh -c '…'` in .woodpecker/build.yml, which
# keeps the step's commands short and testable (scripts/test-deploy-app.sh).
#
# Nothing here reads input: Woodpecker hands the step's script to the shell on
# stdin, and a `docker compose` that read it would eat the rest of the step
# (deploys 385, 390, 391). stdin is detached for everything below.

set -eu
exec < /dev/null

here=$(dirname "$0")

# --- SQL console guards (card #473) -----------------------------------------
#
# Matched the way compose matches it: an exact name in a comma-separated list,
# not a substring (`nosqltool` must not enable it).
#
# The guards are HERE rather than as `:?` in deploy/docker-compose.yml on
# purpose: compose interpolates the whole file before filtering by profile, so
# a `:?` on the profiled service would make these credentials mandatory for
# every deploy. What must never happen is a *fallback* to the application's
# login: that is a superuser, and pgweb's read-only mode does not stop a
# superuser reading host files. Missing means fail, never substitute.
case ",${COMPOSE_PROFILES:-}," in
  *,sqltool,*)
    console=on
    for name in PGWEB_DB_PASSWORD PGWEB_AUTH_USER PGWEB_AUTH_PASS; do
      if [ -z "$(printenv "$name" || true)" ]; then
        echo "ERROR: $name is required when COMPOSE_PROFILES enables sqltool" >&2
        exit 1
      fi
    done
    # One credential, two consumers — pgweb checks it, Traefik injects it — so
    # it is derived here rather than stored twice.
    PGWEB_AUTH_B64=$(printf '%s:%s' "$PGWEB_AUTH_USER" "$PGWEB_AUTH_PASS" | base64 | tr -d '\n')
    export PGWEB_AUTH_B64
    ;;
  *)
    console=off
    # Compose still interpolates the profiled service, so these must resolve
    # to *something*; empty is what an unset variable would give anyway.
    export PGWEB_DB_PASSWORD="${PGWEB_DB_PASSWORD:-}"
    export PGWEB_AUTH_USER="${PGWEB_AUTH_USER:-}"
    export PGWEB_AUTH_PASS="${PGWEB_AUTH_PASS:-}"
    export PGWEB_AUTH_B64=""
    ;;
esac

# --- the backend ------------------------------------------------------------

"$here/migrate-from-surreal.sh" "$@"
# Named services only: with the console's profile active, a bare `up --wait`
# would start pgweb too and wait on it, before its role can log in (the role
# gets its password below, once the backend has run the migration that
# creates it).
"$here/compose-up-with-otel.sh" "$@" up -d --pull always --wait --wait-timeout 120 bored-db bored

# compose-up-with-otel.sh hides the rendered OTEL_* variables from the docker
# CLI it runs; the plain `docker compose` calls below must not see them either
# (the CLI is OpenTelemetry-instrumented and would export its own spans).
for name in $(env | sed -n 's/^\(OTEL_[A-Z0-9_]*\)=.*/\1/p'); do
  unset "$name"
done

# --- the SQL console --------------------------------------------------------

if [ "$console" = on ]; then
  # The migration that creates bored_pgweb ran at backend startup, so the role
  # exists by now; only its password and LOGIN are set here, idempotently, on
  # every deploy — rotating the credential is a change to the stored leaf.
  #
  # Over the database container's local socket, which the postgres image
  # trusts, so no application credential is handled out here at all.
  #
  # Where the password goes, and where it must not:
  # * not on a command line — `-e PGWEB_NEW_PASSWORD` names the variable only,
  #   so docker copies the value from this process's environment and it never
  #   appears in `ps` on the agent host;
  # * not in this script's SQL text — psql's \getenv reads it from that env;
  # * not in Postgres's log — the server does receive the plain statement, and
  #   would log it with the password if logging statements or if it failed, so
  #   this session turns both off first (allowed: `bored` is a superuser).
  # Nothing in this block may echo it either: it does not come from
  # `from_secret`, so Woodpecker does not mask it in the step log.
  echo "==> provisioning the bored_pgweb login"
  export PGWEB_NEW_PASSWORD="$PGWEB_DB_PASSWORD"
  docker compose "$@" exec -T -e PGWEB_NEW_PASSWORD bored-db \
    psql -v ON_ERROR_STOP=1 -q -U bored -d bored <<'SQL'
SET log_statement = 'none';
SET log_min_error_statement = 'panic';
\getenv pgweb_pw PGWEB_NEW_PASSWORD
ALTER ROLE bored_pgweb WITH LOGIN PASSWORD :'pgweb_pw';
SQL
  unset PGWEB_NEW_PASSWORD
  # Recreated so it connects with the credential just set (nothing in the
  # compose model changed, so a plain `up` would leave a stale container).
  echo "==> starting the SQL console"
  docker compose "$@" up -d --force-recreate --no-deps --wait --wait-timeout 60 sqltool
else
  # Switching the profile off has to remove a console that is running: `up`
  # leaves a container whose service dropped out of the active profiles, and a
  # profiled service is not an orphan either. `--profile sqltool` is needed to
  # address it at all; `rm -sf` is a no-op when there is nothing to remove.
  echo "==> ensuring the SQL console is not running"
  docker compose "$@" --profile sqltool rm -sf sqltool
  # And that its role cannot log in: with the console off nothing should be
  # able to use that credential. Idempotent, so it runs on every such deploy;
  # turning the profile back on restores the login (and password) above.
  echo "==> disabling the bored_pgweb login"
  docker compose "$@" exec -T bored-db \
    psql -v ON_ERROR_STOP=1 -q -U bored -d bored -c 'ALTER ROLE bored_pgweb NOLOGIN' < /dev/null
fi
