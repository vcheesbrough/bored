#!/bin/sh
# Tests for scripts/deploy-app.sh (cards #471, #473, #489, #537).
#
# deploy-app.sh is copied into a scratch tree beside a stand-in for the
# script it runs, and a fake `docker` goes first on PATH. Every stand-in
# drains stdin (as `docker compose` does) and records how it was called, so the
# tests see the order, the arguments, what reached stdin, and whether the
# caller's own stdin survived.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORK_DIR="$(mktemp -d)"
# shellcheck disable=SC2064  # expand WORK_DIR now, not at trap time
trap "rm -rf '$WORK_DIR'" EXIT
mkdir -p "$WORK_DIR/scripts" "$WORK_DIR/bin"
cp "$SCRIPT_DIR/deploy-app.sh" "$WORK_DIR/scripts/"

for name in compose-up-with-otel; do
  cat > "$WORK_DIR/scripts/$name.sh" <<FAKE
#!/bin/sh
cat > /dev/null
echo "$name \$*" >> "$WORK_DIR/calls"
exit "\$(cat "$WORK_DIR/$name.status" 2>/dev/null || echo 0)"
FAKE
  chmod +x "$WORK_DIR/scripts/$name.sh"
done
cat > "$WORK_DIR/bin/docker" <<FAKE
#!/bin/sh
cat > "$WORK_DIR/docker-stdin-\$\$"
cat "$WORK_DIR/docker-stdin-\$\$" >> "$WORK_DIR/docker-stdin"
echo "docker \$*" >> "$WORK_DIR/calls"
env | grep '^OTEL_' >> "$WORK_DIR/docker-otel" || true
env | grep '^PGWEB_AUTH_B64=' >> "$WORK_DIR/docker-b64" || true
env | grep '^PGWEB_NEW_PASSWORD=' >> "$WORK_DIR/docker-pw" || true
# The database volume exists unless a test says otherwise (card #537).
case "\$*" in
  "volume inspect "*) [ ! -e "$WORK_DIR/volume-absent" ] || exit 1 ;;
esac
exit 0
FAKE
chmod +x "$WORK_DIR/bin/docker"

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

reset() {
  : > "$WORK_DIR/calls"
  : > "$WORK_DIR/docker-stdin"
  : > "$WORK_DIR/docker-otel"
  : > "$WORK_DIR/docker-b64"
  : > "$WORK_DIR/docker-pw"
  rm -f "$WORK_DIR"/*.status "$WORK_DIR/volume-absent"
}

# Run deploy-app.sh with extra environment given as arguments to `env`.
# PG_VOLUME is set as the pipeline sets it; a test can override it.
run() {
  status=0
  env PATH="$WORK_DIR/bin:$PATH" OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4318 PG_VOLUME=bored-dev-pg "$@" \
    sh "$WORK_DIR/scripts/deploy-app.sh" -p bored-dev -f deploy/docker-compose.yml \
    < /dev/null > "$WORK_DIR/out" 2>&1 || status=$?
}

# --- console off: the backend only, and any console removed -------------------
reset
run COMPOSE_PROFILES=
expected="docker volume inspect bored-dev-pg
compose-up-with-otel -p bored-dev -f deploy/docker-compose.yml up -d --pull always --wait --wait-timeout 120 bored-db bored
docker compose -p bored-dev -f deploy/docker-compose.yml --profile sqltool rm -sf sqltool
docker compose -p bored-dev -f deploy/docker-compose.yml exec -T bored-db psql -v ON_ERROR_STOP=1 -q -U bored -d bored -c ALTER ROLE bored_pgweb NOLOGIN"
if [ "$status" = 0 ] && [ "$(cat "$WORK_DIR/calls")" = "$expected" ]; then
  pass "without the profile: volume checked, backend up (named services), console removed, login disabled"
else
  fail "console off: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- console on: provision the role, then start the console -------------------
reset
run COMPOSE_PROFILES=sqltool PGWEB_DB_PASSWORD=db-pw-123 PGWEB_AUTH_USER=console PGWEB_AUTH_PASS=auth-pw
exec_line=$(grep -n 'exec -T -e PGWEB_NEW_PASSWORD bored-db psql' "$WORK_DIR/calls" | cut -d: -f1)
up_line=$(grep -n 'up -d --force-recreate --no-deps --wait --wait-timeout 60 sqltool' "$WORK_DIR/calls" | cut -d: -f1)
backend_line=$(grep -n '^compose-up-with-otel' "$WORK_DIR/calls" | cut -d: -f1)
if [ "$status" = 0 ] && [ -n "$exec_line" ] && [ -n "$up_line" ] \
    && [ "$backend_line" -lt "$exec_line" ] && [ "$exec_line" -lt "$up_line" ]; then
  pass "with the profile: backend, then the role's password, then the console"
else
  fail "console on: status $status, calls: $(cat "$WORK_DIR/calls")"
fi
if grep -q "ALTER ROLE bored_pgweb WITH LOGIN PASSWORD :'pgweb_pw';" "$WORK_DIR/docker-stdin" \
    && grep -q '\\getenv pgweb_pw PGWEB_NEW_PASSWORD' "$WORK_DIR/docker-stdin" \
    && ! grep -q 'db-pw-123' "$WORK_DIR/docker-stdin"; then
  pass "the password reaches psql through \\getenv, never in the SQL text"
else
  fail "provisioning SQL: $(cat "$WORK_DIR/docker-stdin")"
fi
if grep -q 'db-pw-123' "$WORK_DIR/calls"; then
  fail "the password appeared on a docker command line"
else
  pass "the password never appears on a command line"
fi
if grep -qx 'PGWEB_NEW_PASSWORD=db-pw-123' "$WORK_DIR/docker-pw"; then
  pass "docker gets the password through its environment"
else
  fail "docker env PGWEB_NEW_PASSWORD: $(cat "$WORK_DIR/docker-pw")"
fi
if grep -q "SET log_statement = 'none';" "$WORK_DIR/docker-stdin" \
    && grep -q "SET log_min_error_statement = 'panic';" "$WORK_DIR/docker-stdin" \
    && grep -q "SET log_min_duration_statement = -1;" "$WORK_DIR/docker-stdin"; then
  pass "the provisioning session keeps the statement out of the server log"
else
  fail "no log suppression in: $(cat "$WORK_DIR/docker-stdin")"
fi
# base64 of "console:auth-pw", written out by hand.
if grep -qx 'PGWEB_AUTH_B64=Y29uc29sZTphdXRoLXB3' "$WORK_DIR/docker-b64"; then
  pass "the injected Basic credential is derived from PGWEB_AUTH_USER/PASS"
else
  fail "PGWEB_AUTH_B64 seen by docker: $(sort -u "$WORK_DIR/docker-b64")"
fi
if [ -s "$WORK_DIR/docker-otel" ]; then
  fail "docker saw OTEL variables: $(cut -d= -f1 "$WORK_DIR/docker-otel" | sort -u | tr '\n' ' ')"
else
  pass "the docker CLI never sees the OTEL_* variables"
fi

# --- console on without its credentials: fail before touching anything -------
reset
run COMPOSE_PROFILES=sqltool PGWEB_AUTH_USER=console PGWEB_AUTH_PASS=auth-pw
if [ "$status" = 1 ] && [ ! -s "$WORK_DIR/calls" ] && grep -q 'PGWEB_DB_PASSWORD is required' "$WORK_DIR/out"; then
  pass "a missing console credential fails the deploy before anything runs"
else
  fail "missing credential: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- a console password that would break pgweb's URL is refused --------------
reset
run COMPOSE_PROFILES=sqltool PGWEB_DB_PASSWORD='p@ss/word' PGWEB_AUTH_USER=console PGWEB_AUTH_PASS=auth-pw
if [ "$status" = 1 ] && [ ! -s "$WORK_DIR/calls" ] && grep -q 'it goes into a URL' "$WORK_DIR/out"; then
  pass "a console password with URL-reserved characters is refused up front"
else
  fail "url-unsafe password: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- a name that merely contains `sqltool` does not enable it ----------------
reset
run COMPOSE_PROFILES=nosqltool
if [ "$status" = 0 ] && grep -q 'rm -sf sqltool' "$WORK_DIR/calls"; then
  pass "profiles are matched exactly, not by substring"
else
  fail "nosqltool: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- a failed backend up stops before the console is touched ----------------
reset
echo 1 > "$WORK_DIR/compose-up-with-otel.status"
run COMPOSE_PROFILES=sqltool PGWEB_DB_PASSWORD=db-pw-123 PGWEB_AUTH_USER=console PGWEB_AUTH_PASS=auth-pw
if [ "$status" = 1 ] && [ "$(grep -c '' "$WORK_DIR/calls")" = 2 ] \
    && [ "$(tail -n 1 "$WORK_DIR/calls" | cut -d' ' -f1)" = compose-up-with-otel ]; then
  pass "a failed backend up fails the deploy before the role or console is touched"
else
  fail "failed backend up: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- the database volume (card #537) ------------------------------------------

# Absent: refuse, before compose (and so Postgres) is started at all.
reset
touch "$WORK_DIR/volume-absent"
run COMPOSE_PROFILES=
if [ "$status" = 1 ] && [ "$(cat "$WORK_DIR/calls")" = "docker volume inspect bored-dev-pg" ] \
    && grep -q 'database volume bored-dev-pg does not exist' "$WORK_DIR/out"; then
  pass "a missing database volume fails the deploy before anything starts"
else
  fail "volume absent: status $status, calls: $(cat "$WORK_DIR/calls"), out: $(cat "$WORK_DIR/out")"
fi

# Absent, and the override is anything but exactly `1`: still refuse.
for value in 0 yes true ""; do
  reset
  touch "$WORK_DIR/volume-absent"
  run COMPOSE_PROFILES= BORED_ALLOW_EMPTY_DB="$value"
  if [ "$status" = 1 ] && ! grep -q 'volume create' "$WORK_DIR/calls"; then
    pass "BORED_ALLOW_EMPTY_DB='$value' is not the override"
  else
    fail "BORED_ALLOW_EMPTY_DB='$value': status $status, calls: $(cat "$WORK_DIR/calls")"
  fi
done

# Absent with the override: create it, then carry on with the deploy.
reset
touch "$WORK_DIR/volume-absent"
run COMPOSE_PROFILES=sqltool PGWEB_DB_PASSWORD=db-pw-123 PGWEB_AUTH_USER=console PGWEB_AUTH_PASS=auth-pw BORED_ALLOW_EMPTY_DB=1
create_line=$(grep -nx 'docker volume create bored-dev-pg' "$WORK_DIR/calls" | cut -d: -f1)
backend_line=$(grep -n '^compose-up-with-otel' "$WORK_DIR/calls" | cut -d: -f1)
if [ "$status" = 0 ] && [ -n "$create_line" ] && [ -n "$backend_line" ] \
    && [ "$create_line" -lt "$backend_line" ] && grep -q 'sqltool$' "$WORK_DIR/calls"; then
  pass "BORED_ALLOW_EMPTY_DB=1 creates the missing volume, then deploys"
else
  fail "override: status $status, calls: $(cat "$WORK_DIR/calls")"
fi
if [ -s "$WORK_DIR/docker-otel" ]; then
  fail "the volume calls saw OTEL variables: $(cut -d= -f1 "$WORK_DIR/docker-otel" | sort -u | tr '\n' ' ')"
else
  pass "the volume calls never see the OTEL_* variables either"
fi

# Present with the override: the override does nothing.
reset
run COMPOSE_PROFILES= BORED_ALLOW_EMPTY_DB=1
if [ "$status" = 0 ] && ! grep -q 'volume create' "$WORK_DIR/calls" \
    && grep -q '^compose-up-with-otel' "$WORK_DIR/calls"; then
  pass "an existing volume is used as it is, override or not"
else
  fail "present + override: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# No PG_VOLUME (empty, as unset would be): fail before touching anything.
reset
run PG_VOLUME= COMPOSE_PROFILES=
if [ "$status" != 0 ] && [ ! -s "$WORK_DIR/calls" ] && grep -q 'PG_VOLUME is required' "$WORK_DIR/out"; then
  pass "a deploy without PG_VOLUME fails before anything runs"
else
  fail "no PG_VOLUME: status $status, calls: $(cat "$WORK_DIR/calls")"
fi

# --- the caller's stdin is left untouched ------------------------------------
reset
left=$(printf 'echo the-rest-of-the-step\n' | {
  PATH="$WORK_DIR/bin:$PATH" PG_VOLUME=bored-dev-pg COMPOSE_PROFILES=sqltool PGWEB_DB_PASSWORD=x PGWEB_AUTH_USER=u PGWEB_AUTH_PASS=p \
    sh "$WORK_DIR/scripts/deploy-app.sh" -p bored-dev -f deploy/docker-compose.yml > /dev/null 2>&1
  cat
})
if [ "$left" = "echo the-rest-of-the-step" ]; then
  pass "nothing reads the caller's stdin"
else
  fail "stdin was consumed: left '$left'"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all deploy-app.sh tests passed"
