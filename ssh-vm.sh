#!/usr/bin/env bash
# SSH into the hanui dev VM. Pass extra args/commands through.
set -euo pipefail
cd "$(dirname "$0")"

SSH_PORT="${SSH_PORT:-2222}"
exec ssh \
  -p "$SSH_PORT" \
  -i vm/keys/id_ed25519 \
  -o UserKnownHostsFile=vm/known_hosts \
  -o StrictHostKeyChecking=accept-new \
  -o LogLevel=ERROR \
  dev@127.0.0.1 "$@"
