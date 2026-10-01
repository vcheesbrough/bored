#!/bin/sh
# Prints the release tag for this Woodpecker pipeline: MAJOR.MINOR from the
# workspace Cargo.toml, PATCH = the number of the push pipeline that built (or
# builds) the image. Card #461.
#
#   - push / manual: this pipeline builds the image, so PATCH is its own
#     CI_PIPELINE_NUMBER.
#   - deployment: a deployment is promoted from the push pipeline that built
#     the commit, and Woodpecker gives that pipeline's number as
#     CI_PIPELINE_PARENT — so PATCH is the parent's number, and the deploy
#     names exactly the image that pipeline published. A deployment with no
#     parent (0 or unset) has no image to name and is refused.
#
# Pipeline numbers are unique and increasing per repo, so no two builds share
# a version and nothing has to be reserved in git first: the git tag is pushed
# only after a successful deploy.
#
# Usage: compute-release-tag.sh [path/to/Cargo.toml]   (default: ./Cargo.toml)
# Reads CI_PIPELINE_EVENT, CI_PIPELINE_NUMBER, CI_PIPELINE_PARENT.

set -eu

cargo_toml="${1:-Cargo.toml}"

# MAJOR.MINOR of `[workspace.package].version`; its patch is a placeholder.
# Only that table is searched: a member's or dependency's `version =` must not
# be picked up.
major_minor="$(
    sed -n '/^\[workspace\.package\]/,/^\[/{
        s/^version[[:space:]]*=[[:space:]]*"\([0-9][0-9]*\.[0-9][0-9]*\)\.[0-9][0-9]*"[[:space:]]*$/\1/p
    }' "$cargo_toml"
)"
if [ -z "$major_minor" ]; then
    echo "ERROR: no MAJOR.MINOR.PATCH version in [workspace.package] of $cargo_toml" >&2
    exit 1
fi

case "${CI_PIPELINE_EVENT:-}" in
    push | manual)
        build_number="${CI_PIPELINE_NUMBER:-}"
        source_var=CI_PIPELINE_NUMBER
        ;;
    deployment)
        build_number="${CI_PIPELINE_PARENT:-}"
        source_var=CI_PIPELINE_PARENT
        ;;
    *)
        echo "ERROR: unsupported CI_PIPELINE_EVENT '${CI_PIPELINE_EVENT:-}' (expected push, manual or deployment)" >&2
        exit 1
        ;;
esac

# A positive integer without leading zeros, so the tag is valid semver.
case "$build_number" in
    '' | 0* | *[!0-9]*)
        if [ "$source_var" = CI_PIPELINE_PARENT ]; then
            echo "ERROR: deployment has no parent pipeline (CI_PIPELINE_PARENT='$build_number')." >&2
            echo "Deploy by promoting the push pipeline that built this commit, so the deploy knows which image to use." >&2
        else
            echo "ERROR: $source_var='$build_number' is not a pipeline number" >&2
        fi
        exit 1
        ;;
esac

echo "$major_minor.$build_number"
