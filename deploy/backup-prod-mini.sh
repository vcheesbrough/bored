#!/usr/bin/env bash
# Snapshot SurrealKV data from production Docker volume on mini (see deploy/docker-compose.yml).
#
# Requires: SSH access where HOST resolves (default from WSL: vincent@mini.home).
# Optional: BORED_DB_VOLUME — explicit Docker volume name; otherwise picks first
#           `docker volume ls` name matching `bored` (case-insensitive).
#
# Usage:
#   chmod +x deploy/backup-prod-mini.sh
#   ./deploy/backup-prod-mini.sh
#   BORED_DB_VOLUME=my_external_volume ./deploy/backup-prod-mini.sh
#   SSH_TARGET=vincent@mini ./deploy/backup-prod-mini.sh

set -euo pipefail

SSH_TARGET="${SSH_TARGET:-vincent@mini.home}"

REMOTE_SCRIPT=$(cat <<'EOS'
set -euo pipefail
VOL="${BORED_DB_VOLUME:-}"
if [[ -z "${VOL}" ]] && docker volume inspect bored-prod-db &>/dev/null; then
  VOL=bored-prod-db
fi
if [[ -z "${VOL}" ]]; then
  VOL="$(docker volume ls -q | grep -i bored | head -1 || true)"
fi
if [[ -z "${VOL}" ]]; then
  echo "Could not infer volume; set BORED_DB_VOLUME on the remote shell or pass from client." >&2
  docker volume ls >&2
  exit 1
fi

mkdir -p "${HOME}/backups"
TS="$(date -u +%Y%m%d-%H%M%SZ)"
NAME="bored-db-${TS}.tar.gz"
OUT="${HOME}/backups/${NAME}"

docker run --rm \
  -v "${VOL}:/data:ro" \
  -v "${HOME}/backups:/out:rw" \
  alpine@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc \
  tar czf "/out/${NAME}" -C /data bored.db

ls -lh "${OUT}"
printf '%s\n' "${OUT}"
EOS
)

ssh -o BatchMode=yes "$SSH_TARGET" env BORED_DB_VOLUME="${BORED_DB_VOLUME:-}" bash -seuo pipefail <<<"$REMOTE_SCRIPT"
