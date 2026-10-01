#!/bin/sh
# Pushes the image this push pipeline built and e2e-tested, under its version
# and as the commit's latest green build. Card #461.
#
#   1. Refuse if the registry already holds the version. Versions are
#      MAJOR.MINOR.<pipeline number>, unique only while Woodpecker's pipeline
#      counter is: if the repo is re-created in Woodpecker or its DB restored
#      from an old backup, numbers repeat, and pushing would silently replace
#      an image prod or a git tag may name. A registry error in the probe also
#      exits non-zero and falls through to the push, which then fails on its
#      own if the registry is really down.
#   2. Push :<version>.
#   3. Push the same image as :commit-<sha>, which a deployment of a commit
#      never deployed before resolves to (resolve-deploy-tag.sh). A re-run of
#      the commit's push pipeline moves it to the newer build.
#
# Usage: publish-release-image.sh <version> <commit-sha>
# Reads IMAGE_REPO (default registry.desync.link/bored); needs a logged-in
# docker holding the built <repo>:<version> image.

set -eu

version="${1:?usage: publish-release-image.sh <version> <commit-sha>}"
sha="${2:?usage: publish-release-image.sh <version> <commit-sha>}"
image_repo="${IMAGE_REPO:-registry.desync.link/bored}"

image="$image_repo:$version"
commit_image="$image_repo:commit-$sha"

if docker manifest inspect "$image" >/dev/null 2>&1; then
    echo "ERROR: $image already exists in the registry; Woodpecker's pipeline numbers have repeated (card #461)." >&2
    echo "Refusing to overwrite it. Start the next iteration — its new minor (AGENTS.md: minor = iteration) is a" >&2
    echo "fresh version line — rather than bumping the minor mid-iteration." >&2
    exit 1
fi

docker push "$image"
docker tag "$image" "$commit_image"
docker push "$commit_image"
