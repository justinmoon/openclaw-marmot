#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

STATE_DIR="${STATE_DIR:-.state}"
RELAY_URL="${RELAY_URL:-}"

rm -rf "${STATE_DIR}"
mkdir -p "${STATE_DIR}/relay/nostr-rs-relay-db"

docker compose down -v --remove-orphans >/dev/null 2>&1 || true
docker pull scsibug/nostr-rs-relay:latest >/dev/null

IMAGE_DIGEST="$(docker image inspect --format '{{index .RepoDigests 0}}' scsibug/nostr-rs-relay:latest || true)"
if [[ -n "${IMAGE_DIGEST}" ]]; then
  printf '%s\n' "${IMAGE_DIGEST}" > RELAY_IMAGE.txt
fi

docker compose up -d

if [[ -z "${RELAY_URL}" ]]; then
  HOSTPORT_LINE="$(docker compose port relay 8080 | head -n 1)"
  HOSTPORT="${HOSTPORT_LINE##*:}"
  if [[ -z "${HOSTPORT}" ]]; then
    echo "failed to resolve relay port from: ${HOSTPORT_LINE}" >&2
    exit 1
  fi
  RELAY_URL="ws://127.0.0.1:${HOSTPORT}"
fi

cleanup() {
  docker compose down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

cargo run -p marmotd -- scenario invite-and-chat-daemon --relay "${RELAY_URL}" --state-dir "${STATE_DIR}"
