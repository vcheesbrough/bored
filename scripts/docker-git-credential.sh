#!/bin/sh
# Sourced (not executed) by the cargo layers in the Dockerfile.
#
# sovereign-config-provider is a `git = "https://github.com/vcheesbrough/sovereign-config"`
# dependency on a PRIVATE repo, so `cargo fetch` inside the build container needs a
# credential. BuildKit mounts the token at /run/secrets/github_token
# (`docker build --secret id=github_token,env=GITHUB_TOKEN`).
#
# The rewrite is exported as GIT_CONFIG_* env vars rather than written with
# `git config --global`, so the token never lands in a filesystem layer.
# No secret → no rewrite: the build still runs and simply fails on the private
# fetch with a plain auth error rather than a confusing missing-mount error.

if [ -s /run/secrets/github_token ]; then
    GIT_CONFIG_COUNT=1
    GIT_CONFIG_KEY_0="url.https://x-access-token:$(cat /run/secrets/github_token)@github.com/.insteadOf"
    GIT_CONFIG_VALUE_0="https://github.com/"
    export GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
else
    echo "WARNING: /run/secrets/github_token absent; private git deps will fail to fetch" >&2
fi
