#!/bin/sh
# Removes e2e compose projects left behind by cancelled or timed-out pipelines.
# Card #461.
#
# CI runs each pipeline's e2e stack as its own project, bored-e2e-<pipeline
# number>, so concurrent pipelines cannot tear down each other's stacks. The
# price is that nothing reuses a leaked project: a pipeline killed before its
# `down` leaves containers, a network and volumes behind for good, and every
# leaked network eats one of the daemon's address pools until `compose up`
# fails for every project on the host (v-note and the deploys included).
#
# Only projects whose containers have all existed for hours are removed. An
# e2e run takes minutes, so that can never be a live pipeline's stack, which a
# blanket `bored-e2e-*` cleanup would kill. Docker's RunningFor reads
# "About an hour ago" up to ~90 minutes and "N hours/days/weeks/months/years
# ago" after, so matching those plural units means at least ~90 minutes old.
#
# Best effort: a failure to remove something is a warning, never a failed run.
#
# Usage: prune-stale-e2e-projects.sh <current-project>
# Needs docker.

set -eu

current="${1:?usage: prune-stale-e2e-projects.sh <current-project>}"

listing="$(docker ps -a --filter label=com.docker.compose.project \
    --format '{{.Label "com.docker.compose.project"}}|{{.RunningFor}}')"

# Projects named bored-e2e-<digits>, other than this run's, where no
# container is younger than the threshold.
stale="$(
    printf '%s\n' "$listing" | awk -F'|' -v current="$current" '
        $1 ~ /^bored-e2e-[0-9]+$/ && $1 != current {
            seen[$1] = 1
            if ($2 !~ /(hours|days|weeks|months|years) ago$/) young[$1] = 1
        }
        END { for (p in seen) if (!(p in young)) print p }
    ' | sort
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
done
