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
#
# A MALFORMED secret, on the other hand, is fatal and is rejected here. A value
# containing whitespace (an error message captured by a command substitution, a
# stray `echo`, a pasted line) builds a GIT_CONFIG_KEY_0 that git cannot parse.
# Left unchecked, the operator sees `fatal: unable to parse command-line config`
# followed by four spurious-network retries and a `revision … not found` against
# sovereign-config — every line pointing at the network or the pinned revision
# rather than at the credential. Failing here names the actual cause and saves
# ~1 minute of retries in the frontend-builder stage.

# Overridable so the script is exercisable outside a BuildKit mount (see
# scripts/test-docker-git-credential.sh). The default is the mount path.
GITHUB_TOKEN_FILE="${GITHUB_TOKEN_FILE:-/run/secrets/github_token}"

if [ -s "$GITHUB_TOKEN_FILE" ]; then
    # Capture before validating. Command substitution strips trailing newlines,
    # so a secret stored with a trailing "\n" — as CI secret stores commonly do
    # — is normalised here and passes the check below, exactly as it did before
    # validation existed.
    _bored_github_token="$(cat "$GITHUB_TOKEN_FILE")"

    # Every GitHub credential form is [A-Za-z0-9_]: the `ghp_`/`gho_`/`ghu_`/
    # `ghs_`/`ghr_` prefixed tokens, `github_pat_` fine-grained tokens, and the
    # legacy 40-char hex tokens. Anything else is not a token — most usefully,
    # this catches whitespace and captured error text.
    case "$_bored_github_token" in
        *[!A-Za-z0-9_]*)
            echo "ERROR: the github_token secret at $GITHUB_TOKEN_FILE is not a valid credential" >&2
            echo "       (it contains whitespace or characters no GitHub token contains)." >&2
            echo "       Check the GITHUB_TOKEN passed to --secret id=github_token,env=GITHUB_TOKEN." >&2
            echo "       A common cause is capturing an error message, e.g. \`gh auth token\` on gh < 2.5.0." >&2
            unset _bored_github_token
            # Sourced, so `return` — not `exit` — hands the failure to the
            # caller's `set -e` without killing an interactive shell.
            return 1
            ;;
    esac

    GIT_CONFIG_COUNT=1
    GIT_CONFIG_KEY_0="url.https://x-access-token:${_bored_github_token}@github.com/.insteadOf"
    GIT_CONFIG_VALUE_0="https://github.com/"
    export GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
    unset _bored_github_token
else
    echo "WARNING: $GITHUB_TOKEN_FILE absent; private git deps will fail to fetch" >&2
fi
