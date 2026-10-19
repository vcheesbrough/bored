#!/usr/bin/env bash
# Attach the SQL console's proxy provider to the shared Authentik outpost (card #473, ported from v-note #299).
#
# Why this is a script and not a blueprint entry — the whole point:
#
# A proxy provider does nothing until an outpost serves it. The forward-auth
# endpoint answers 404 for a host no attached provider claims, which is why
# the blueprints alone leave /dbconsole unreachable.
#
# The obvious fix — declaring `authentik_outposts.outpost` in that blueprint —
# is a trap. The embedded outpost is SHARED: its `providers` list also carries
# the providers protecting glances, woodpecker, uptime-kuma, the Traefik
# dashboard, v-note's console and the rest. A blueprint sets a list wholesale
# rather than appending, so that entry would detach all of them, and the
# blueprint is applied by *every deployment*, dev ones from any branch. One
# careless entry, LAN-wide auth outage.
#
# So this does an explicit read-modify-write instead: read the list, append our
# provider if it is missing, write it back — and refuse to write anything that
# would remove a provider it did not add. It is idempotent, so it runs on every
# deploy and is a no-op on all but the first.
#
# Required environment:
#   AUTHENTIK_URL     base URL, e.g. https://auth.desync.link
#   AUTHENTIK_TOKEN   API token with permission to read providers and patch outposts
#   PROVIDER_NAME     the proxy provider to attach, e.g. bored-sql-console-dev
# Optional:
#   OUTPOST_NAME      defaults to "authentik Embedded Outpost"

set -euo pipefail

OUTPOST_NAME="${OUTPOST_NAME:-authentik Embedded Outpost}"

for name in AUTHENTIK_URL AUTHENTIK_TOKEN PROVIDER_NAME; do
  if [ -z "$(printenv "$name" || true)" ]; then
    echo "ERROR: $name is required" >&2
    exit 1
  fi
done

api() {
  method="$1"
  path="$2"
  shift 2
  curl -sS --fail-with-body -X "$method" \
    -H "Authorization: Bearer ${AUTHENTIK_TOKEN}" \
    -H "Content-Type: application/json" \
    "${AUTHENTIK_URL%/}/api/v3${path}" "$@"
}

# jq rather than python3 so the pipeline step needs only curl and jq. The
# outpost name contains a space, so this is not optional.
urlencode() {
  jq -rn --arg s "$1" '$s | @uri'
}

echo "==> looking up proxy provider '${PROVIDER_NAME}'"
provider_pk=$(
  api GET "/providers/proxy/?name__iexact=$(urlencode "$PROVIDER_NAME")" |
    jq -r '.results[0].pk // empty'
)
if [ -z "$provider_pk" ]; then
  echo "ERROR: no proxy provider named '${PROVIDER_NAME}'" >&2
  echo "       The blueprint apply that creates it must run before this step." >&2
  exit 1
fi
echo "    provider pk=${provider_pk}"

echo "==> looking up outpost '${OUTPOST_NAME}'"
outpost=$(
  api GET "/outposts/instances/?name__iexact=$(urlencode "$OUTPOST_NAME")" |
    jq -c '.results[0] // empty'
)
if [ -z "$outpost" ]; then
  echo "ERROR: no outpost named '${OUTPOST_NAME}'" >&2
  exit 1
fi

outpost_pk=$(printf '%s' "$outpost" | jq -r '.pk')
before=$(printf '%s' "$outpost" | jq -c '.providers // []')
echo "    outpost pk=${outpost_pk}, currently serving $(printf '%s' "$before" | jq 'length') provider(s)"

if printf '%s' "$before" | jq -e --argjson pk "$provider_pk" 'index($pk) != null' >/dev/null; then
  echo "==> already attached — nothing to do"
  exit 0
fi

# Re-read immediately before the write rather than reusing `$before` from the
# lookup above. The PATCH sends the whole list, and other deployments (bored's
# other environment, v-note's) attach their own providers the same way, so two
# overlapping runs can both read the same list and have the second clobber what
# the first added.
#
# ⚠️ This NARROWS that window; it does not close it. The endpoint offers no
# compare-and-swap, so an interleaving between this read and the PATCH below is
# still possible. The post-write verification is what actually catches the
# damage — and it is deliberately the only claim made here.
current=$(
  api GET "/outposts/instances/?name__iexact=$(urlencode "$OUTPOST_NAME")" |
    jq -c '.results[0].providers // []'
)
after=$(printf '%s' "$current" | jq -c --argjson pk "$provider_pk" '. + [$pk]')

# (No pre-write "does the new list keep every old entry" check: `$after` is
# built as `$current` plus one id, so it holds by construction and could never
# fail. The verification after the write is the one that can catch damage.)

echo "==> attaching provider ${provider_pk} to outpost ${outpost_pk}"
api PATCH "/outposts/instances/${outpost_pk}/" \
  --data "$(jq -nc --argjson providers "$after" '{providers: $providers}')" >/dev/null

# Read back rather than trusting the write: this is shared state, and "the PATCH
# returned 200" is not the same claim as "the provider is now served".
verified=$(
  api GET "/outposts/instances/?name__iexact=$(urlencode "$OUTPOST_NAME")" |
    jq -c '.results[0].providers // []'
)
if ! printf '%s' "$verified" | jq -e --argjson pk "$provider_pk" 'index($pk) != null' >/dev/null; then
  echo "ERROR: provider ${provider_pk} is still not attached after the patch" >&2
  echo "       outpost now serves: ${verified}" >&2
  exit 1
fi

# The assertion this whole script is about, and the one the check above does NOT
# make: every provider attached before is still attached. "Our provider is
# present" says nothing about the eleven protecting glances, woodpecker,
# uptime-kuma and the Traefik dashboard — a write that added ours and dropped
# all of theirs satisfies it completely.
if ! printf '%s' "$verified" | jq -e --argjson current "$current" \
  '($current - .) == []' >/dev/null; then
  echo "ERROR: the outpost lost providers it had before this patch" >&2
  echo "       before=${current}" >&2
  echo "       now=${verified}" >&2
  # Put them back straight away rather than leave every other LAN service
  # without auth until someone notices: write the union of what was there
  # before, what is there now (a concurrent attach may have added its own),
  # and ours — then re-check. The step still fails either way, so the
  # incident is seen.
  restore=$(jq -nc --argjson a "$current" --argjson b "$verified" --argjson pk "$provider_pk" \
    '($a + $b + [$pk]) | unique')
  echo "==> restoring the outpost's providers: ${restore}" >&2
  api PATCH "/outposts/instances/${outpost_pk}/" \
    --data "$(jq -nc --argjson providers "$restore" '{providers: $providers}')" >/dev/null || true
  restored=$(
    api GET "/outposts/instances/?name__iexact=$(urlencode "$OUTPOST_NAME")" |
      jq -c '.results[0].providers // []'
  )
  if printf '%s' "$restored" | jq -e --argjson want "$restore" '($want - .) == []' >/dev/null; then
    echo "       restored; the outpost serves ${restored}" >&2
  else
    echo "ERROR: the restore did not take — restore by hand: providers=${restore}" >&2
  fi
  exit 1
fi

echo "==> attached; outpost now serves $(printf '%s' "$verified" | jq 'length') provider(s)"
