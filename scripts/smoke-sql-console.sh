#!/usr/bin/env bash
# Post-deploy smoke check: is the SQL console gated, and did gating it break the app?
#
# Why this exists: the console's whole safety story is two Traefik labels and a
# provider attachment, none of which any CI stack can exercise —
# e2e/docker-compose.test.yml has a Traefik edge but no Authentik, and does not
# route /dbconsole through it. The e2e suite
# proves the *database* boundary holds (read-only role, no superuser, session
# locked); this proves the *network* boundary holds, and it is the only thing
# that does.
#
# Three assertions, in the order they can fail:
#
#   1. /dbconsole/ must NOT serve the console to an anonymous request. A 302 to
#      Authentik is the expected answer.
#   2. /dbconsole/ must not 404 either. That is what an unattached provider looks
#      like (the forward-auth endpoint answers 404 for a host no provider
#      claims), and it is "the console is broken", not "the console is safe" —
#      passing on it would let the feature silently never work.
#   3. The SPA on the same host must still load. Two Authentik integrations now
#      share one origin, which is the part of this console most likely to misbehave, and
#      a console that works by breaking login is not a trade worth making.
#
# No credentials and no session: this stops at the redirect rather than
# authenticating, so it needs no test user and is safe against any environment.
#
# Usage: BORED_HOST=bored-dev.desync.link ./scripts/smoke-sql-console.sh

set -euo pipefail

HOST="${BORED_HOST:?BORED_HOST must be set (e.g. bored-dev.desync.link)}"

# Nothing to check when the console is not deployed — and checking anyway would
# be actively harmful. With the profile off, /dbconsole stops matching the
# console router and falls through to the app's `Host(…)` router, where the
# SPA's catch-all serves index.html with a 200. This check reads a 200 as
# "publicly readable" and fails, which would then fail every subsequent dev
# deployment — so documenting "remove COMPOSE_PROFILES to disable the console"
# would have bricked deploys.
#
# The value is shared with the deploy step through a YAML anchor in
# .woodpecker/build.yml, so enabling and disabling stays a single edit.
# Exact comma-separated match, as compose does it and as deploy-app.sh does
# it — the two must agree about whether the console is deployed, or this check
# runs against a stack that has none (or skips one that does).
case ",${COMPOSE_PROFILES:-}," in
  *,sqltool,*) ;;
  *)
    echo "==> COMPOSE_PROFILES does not enable the SQL console — nothing to check"
    exit 0
    ;;
esac
ATTEMPTS="${SMOKE_ATTEMPTS:-10}"
DELAY="${SMOKE_DELAY:-6}"
# scripts/test-smoke-sql-console.sh overrides these to drive the check against a
# local stub so its failure modes can be tested without a deployment.
SCHEME="${SMOKE_SCHEME:-https}"
IDP_HOST="${SMOKE_IDP_HOST:-auth.desync.link}"

fail() {
  echo "FAIL — $1" >&2
  exit 1
}

# The console container is recreated at the end of the deploy and takes a moment
# to accept connections, so a first probe can legitimately fail to connect.
# Status and Location are read together: asking twice would let the two answers
# disagree. `|| true` keeps a refused connection from killing the loop.
#
# 404 is retried, and that is not cosmetic. The deploy attaches the proxy
# provider to the embedded outpost moments earlier, and the outpost picks that
# change up asynchronously — until it does, forward auth answers 404 for this
# host. Observed on the first real deploy: 404 at 29s, correct 302 shortly
# after. A 404 that survives every attempt still fails, because a permanently
# unattached provider means the console never works.
status=000
location=""
for attempt in $(seq 1 "$ATTEMPTS"); do
  probe=$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' --max-time 15 \
    "$SCHEME://$HOST/dbconsole/" 2>/dev/null || true)
  status="${probe%% *}"
  [ -n "$status" ] || status=000
  location="${probe#* }"

  case "$status" in
    000 | 404 | 502 | 503 | 504) ;;
    *) break ;;
  esac

  echo "  attempt $attempt/$ATTEMPTS: /dbconsole/ returned $status, retrying in ${DELAY}s"
  sleep "$DELAY"
done

echo "==> GET $SCHEME://$HOST/dbconsole/ → $status"
[ -n "$location" ] && echo "    $location"

# The assertion the whole card rests on.
case "$status" in
  200)
    fail "/dbconsole/ served the console to an ANONYMOUS request — it is publicly readable"
    ;;
  404)
    fail "/dbconsole/ returned 404 — the proxy provider is not attached to the outpost,
       so the console is unreachable. Run scripts/attach-sqltool-outpost.sh, or check
       that the blueprint apply created bored-sql-console-<env>."
    ;;
  302 | 303) ;;
  401)
    # pgweb's own basic auth answered instead of Authentik. Safe, but it means
    # the forward-auth middleware never ran, so the identity gate is missing.
    fail "/dbconsole/ returned 401 from pgweb rather than redirecting to Authentik —
       the authentik@docker middleware is not on the console router. The backstop
       held, but there is no identity gate in front of it."
    ;;
  *)
    fail "/dbconsole/ returned an unexpected status $status"
    ;;
esac

case "$location" in
  *"$IDP_HOST"*) ;;
  *) fail "/dbconsole/ redirected somewhere other than $IDP_HOST: $location" ;;
esac
echo "  ok   — an anonymous request is redirected to $IDP_HOST, not served"

# The callback router. Without it the redirect above completes into the
# SPA's catch-all and login loops forever, which is invisible from step 1 alone.
callback_status=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 15 \
  "$SCHEME://$HOST/outpost.goauthentik.io/start" 2>/dev/null || true)
echo "==> GET $SCHEME://$HOST/outpost.goauthentik.io/start → $callback_status"
case "$callback_status" in
  200)
    fail "the outpost path returned 200 — that is the SPA's catch-all answering, so the
       callback router is missing and a console login can never complete."
    ;;
  302 | 303 | 400 | 401 | 403) ;;
  *) fail "the outpost path returned an unexpected status $callback_status" ;;
esac
echo "  ok   — the outpost path is handled by the auth layer, not the SPA"

# Two Authentik integrations now share this origin. The SPA must be untouched.
app_status=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 15 \
  "$SCHEME://$HOST/" 2>/dev/null || true)
echo "==> GET $SCHEME://$HOST/ → $app_status"
[ "$app_status" = "200" ] ||
  fail "the SPA no longer serves on $HOST (status $app_status) — gating /dbconsole
       has affected the application's own routing"
echo "  ok   — the SPA still serves on the same host"

echo
echo "SQL console smoke check OK"
