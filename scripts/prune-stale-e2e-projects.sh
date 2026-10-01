#!/bin/sh
# Removes e2e compose projects left behind by cancelled or timed-out pipelines.
# Card #461.
#
# CI runs each pipeline's e2e stack as its own project, bored-e2e-<pipeline
# number>, so concurrent pipelines cannot tear down each other's stacks. The
# price is that nothing reuses a leaked project: a pipeline killed before its
# `down` leaves containers, a network and volumes behind for good, and every
# leaked network eats one of the daemon's address pools until `compose up`
# fails for every project on the host (v-note and the deploys included), and
# its built images pile up on disk.
#
# Only projects that have existed for ~90 minutes or more are removed. An e2e
# run takes minutes, so that can never be a live pipeline's stack, which a
# blanket `bored-e2e-*` cleanup would kill:
#   - a project with containers is stale when all of them are that old.
#     Docker's RunningFor reads "About an hour ago" up to ~90 minutes and
#     "N hours/days/weeks/months/years ago" after, so those plural units;
#   - a project with no containers at all (cancelled during `up`, after its
#     network was created) is stale when its network is that old, by the
#     network's own creation time.
#
# Best effort: a failure to remove something is a warning, never a failed run.
#
# Usage: prune-stale-e2e-projects.sh <current-project>
# Needs docker.

set -eu

current="${1:?usage: prune-stale-e2e-projects.sh <current-project>}"

threshold=5400  # seconds: ~90 minutes, as RunningFor's plural units
now="$(date +%s)"

containers_listing="$(docker ps -a --filter label=com.docker.compose.project \
    --format '{{.Label "com.docker.compose.project"}}|{{.RunningFor}}')"
network_ids="$(docker network ls -q --filter label=com.docker.compose.project)"
# One inspect per network: a batch inspect fails outright if any one network
# vanished since the listing (another pipeline's `down`, likely with
# concurrent pipelines), which would skip the whole cleanup. A vanished one
# is simply skipped.
networks_listing="$(
    for id in $network_ids; do
        docker network inspect \
            -f '{{index .Labels "com.docker.compose.project"}}|{{.Created.Unix}}' "$id" 2>/dev/null || true
    done
)"

# Projects named bored-e2e-<digits>, other than this run's, that are stale by
# the rules above.
stale="$(
    {
        printf '%s\n' "$containers_listing" | sed 's/^/C|/'
        printf '%s\n' "$networks_listing" | sed 's/^/N|/'
    } | awk -F'|' -v current="$current" -v now="$now" -v threshold="$threshold" '
        $2 !~ /^bored-e2e-[0-9]+$/ || $2 == current { next }
        $1 == "C" {
            has_container[$2] = 1
            if ($3 !~ /(hours|days|weeks|months|years) ago$/) young[$2] = 1
        }
        $1 == "N" && now - $3 >= threshold { old_network[$2] = 1 }
        END {
            for (p in has_container) if (!(p in young)) print p
            for (p in old_network) if (!(p in has_container)) print p
        }
    ' | sort -u
)"

for project in $stale; do
    echo "removing stale e2e project $project"
    label="label=com.docker.compose.project=$project"
    containers="$(docker ps -aq --filter "$label")"
    [ -z "$containers" ] || docker rm -f $containers >/dev/null \
        || echo "WARNING: could not remove containers of $project" >&2
    networks="$(docker network ls -q --filter "$label")"
    [ -z "$networks" ] || docker network rm $networks >/dev/null \
        || echo "WARNING: could not remove networks of $project" >&2
    volumes="$(docker volume ls -q --filter "$label")"
    [ -z "$volumes" ] || docker volume rm $volumes >/dev/null \
        || echo "WARNING: could not remove volumes of $project" >&2
    # The images compose built for the project, named <project>-<service>
    # (the run's own `down --rmi local` never ran). The trailing "-" keeps
    # bored-e2e-35 from matching bored-e2e-350's images.
    images="$(docker image ls -q --filter "reference=$project-*")"
    [ -z "$images" ] || docker image rm -f $images >/dev/null \
        || echo "WARNING: could not remove images of $project" >&2
done
