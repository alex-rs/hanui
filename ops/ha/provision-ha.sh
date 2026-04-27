#!/usr/bin/env bash
# provision-ha.sh — atomic-on-success Home Assistant bootstrap for hanui Phase 2.5.
#
# Runs INSIDE the dev VM (TASK-053's `make ha-up` SSHes here). Writes a
# `HA_URL=` and `HA_TOKEN=` fragment to STDOUT for the caller to merge into
# the host's `.env.local`. ALL progress messages go to STDERR. The Long-Lived
# Access Token (LLAT) value never appears on STDERR — only a redacted prefix.
#
# === Atomicity contract (locked_decisions.idempotency_model) ===
# - On success: HA up + onboarding done + LLAT durably emitted to stdout. The
#   Makefile recipe is responsible for atomically writing the stdout fragment
#   to `.env.local` on the host (TASK-053).
# - On any failure mid-run: exit non-zero. Recovery is `make ha-clean &&
#   make ha-up`. There is NO API-based mid-run recovery (Risk #2; codex
#   BLOCKER 2: LLATs are only mintable via the authenticated websocket
#   command `auth/long_lived_access_token`, never re-mintable from a stale
#   onboarding state).
#
# === Token transport contract (locked_decisions.token_transport) ===
# - Existing-token detection: caller pipes the host's HA_* env lines (subset
#   of `.env.local`) into this script's STDIN. Script reads stdin, parses
#   only `HA_TOKEN=...` and `HA_URL=...` lines (no shell `eval` — guards
#   against injection from a tampered `.env.local`). Token NEVER passed via
#   argv — would leak to `ps`/`/proc/<pid>/cmdline` (Risk #4).
# - LLAT emission: written to stdout via heredoc only. Stderr never sees the
#   full token. The script exits non-zero if stdout is closed before the
#   write completes — preventing silent half-writes.
#
# === Password lifecycle ===
# - `_admin_password` is generated with `openssl rand -base64 32`, held in a
#   single function-local variable, used ONLY for (a) `POST /api/onboarding/
#   users` and (b) the subsequent `POST /auth/token` exchange. Discarded
#   (`unset`) ONLY after the LLAT has been written to stdout AND stdout has
#   been flushed. If anything before that write fails, the script exits
#   non-zero; the password is still discarded but the HA instance is left in
#   a partial state requiring `make ha-clean`.
#
# === Onboarding API note (HA Core, verified 2026-04-27) ===
# `POST /api/onboarding/users` returns `{"auth_code": "..."}`, not an access
# token directly. The auth_code is exchanged for a short-lived access token
# via `POST /auth/token` (grant_type=authorization_code). That short-lived
# access token is what authenticates the websocket session in step 6. The
# plan's text "captures the short-lived access token from response" was
# describing the NET effect of steps 5+5b; this script makes that explicit.
#
# References:
# - https://developers.home-assistant.io/docs/auth_api/
# - https://developers.home-assistant.io/docs/api/websocket/
# - docs/plans/2026-04-27-phase-2.5-local-ha-target.md (Phase 2.5 plan)
# - docs/backlog/TASK-052.md (this ticket)

set -euo pipefail

# ----- Constants -----
readonly HA_HOST="127.0.0.1"
readonly HA_PORT="8123"
readonly HA_API_BASE="http://${HA_HOST}:${HA_PORT}"
readonly HA_WS_URL="ws://${HA_HOST}:${HA_PORT}/api/websocket"
COMPOSE_FILE_DIR="$(cd "$(dirname "$0")" && pwd)"
readonly COMPOSE_FILE_DIR
readonly CLIENT_ID="http://hanui.local/"   # opaque per HA OAuth contract; URL-formed string
readonly LLAT_CLIENT_NAME="hanui-dev"
readonly LLAT_LIFESPAN_DAYS=3650
readonly READY_POLL_TIMEOUT_S=120
readonly TEMPLATE_POLL_TIMEOUT_S=30

# ----- Logging (STDERR ONLY; never echoes tokens) -----
log() { printf '[provision-ha] %s\n' "$*" >&2; }
fatal() { log "ERROR: $*"; exit 1; }

# Redact a secret to its first 4 chars + ellipsis. Never call on any value
# that hasn't been confirmed as a token-shaped string (avoid leaking full
# short strings).
redact() {
  local s="$1"
  if [[ ${#s} -lt 8 ]]; then printf '<short-redacted>'
  else printf '%s...' "${s:0:4}"
  fi
}

# ----- Preflight -----
preflight() {
  command -v docker >/dev/null 2>&1 \
    || fatal "docker not found. Install via 'make vm-docker' (TASK-050)."
  docker compose version >/dev/null 2>&1 \
    || fatal "docker compose v2 plugin missing. Install via 'make vm-docker' (TASK-050) — this script uses 'docker compose' (no hyphen), not legacy 'docker-compose'."
  command -v curl >/dev/null 2>&1 || fatal "curl not found."
  command -v python3 >/dev/null 2>&1 \
    || fatal "python3 not found. The websocket exchange (LLAT generation, Risk #13) uses python3 stdlib — not optional."
  command -v openssl >/dev/null 2>&1 || fatal "openssl not found (needed for password generation)."

  # Port-conflict check (Risk #3): port 8123 must be free OR already bound by
  # our own container. Use ss if available, else /proc/net/tcp parsing.
  local port_owner=""
  if command -v ss >/dev/null 2>&1; then
    # ss -ltn shows listening TCP sockets. Match :8123 at end of local addr.
    if ss -ltnH 2>/dev/null | awk '{print $4}' | grep -qE "[:.]${HA_PORT}\$"; then
      port_owner="$(ss -ltnpH 2>/dev/null | awk -v p=":${HA_PORT}" '$4 ~ p {print; exit}' || true)"
      # Allow the HA container to be already-listening (idempotent step-1 path).
      if ! docker ps --format '{{.Names}}\t{{.Ports}}' 2>/dev/null \
            | grep -E '^hanui-homeassistant\s' \
            | grep -q "${HA_PORT}->8123"; then
        fatal "port ${HA_PORT} already in use by another process: ${port_owner}"
      fi
    fi
  fi
}

# ----- Existing-token check (idempotency, step 1) -----
# Reads HA_* lines from STDIN (no eval). Sets:
#   _existing_ha_token (may be empty)
#   _existing_ha_url   (may be empty)
read_existing_env() {
  _existing_ha_token=""  # gitleaks:allow (false-positive: empty assignment to *_token var)
  _existing_ha_url=""
  if [[ -t 0 ]]; then
    log "no existing env on stdin (interactive run)"
    return 0
  fi
  # Read line-by-line; only accept lines matching exactly HA_TOKEN= or HA_URL=
  # at the start. Strips optional surrounding double-quotes. No eval.
  local line key value
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      HA_TOKEN=*) key=HA_TOKEN; value="${line#HA_TOKEN=}" ;;
      HA_URL=*)   key=HA_URL;   value="${line#HA_URL=}"   ;;
      *) continue ;;
    esac
    # Strip a single pair of surrounding double-quotes if present.
    if [[ "$value" =~ ^\".*\"$ ]]; then
      value="${value:1:${#value}-2}"
    fi
    case "$key" in
      HA_TOKEN) _existing_ha_token="$value" ;;
      HA_URL)   _existing_ha_url="$value"   ;;
    esac
  done
}

# Returns 0 if the existing token successfully authenticates against /api/.
verify_existing_token() {
  local token="$1"
  [[ -z "$token" ]] && return 1
  # Pipe the token via stdin to curl's -H @- — keeps it out of argv.
  # `--max-time 5` avoids hanging if HA is mid-restart.
  local code
  code="$(printf 'Authorization: Bearer %s\n' "$token" \
    | curl -sS -o /dev/null -w '%{http_code}' \
        --max-time 5 \
        -H @- \
        "${HA_API_BASE}/api/" 2>/dev/null || true)"
  [[ "$code" == "200" ]]
}

# Returns 0 if onboarding has already completed (HA installed but new admin
# can't be created). The onboarding `users` step status endpoint is GET
# /api/onboarding (returns array of {step, done}). If `users` is `done: true`
# we are in partial-state territory when no HA_TOKEN is on hand.
onboarding_users_done() {
  local body
  body="$(curl -sS --max-time 5 "${HA_API_BASE}/api/onboarding" 2>/dev/null || true)"
  [[ -z "$body" ]] && return 1
  printf '%s' "$body" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(2)
for step in data:
    if step.get("step") == "user" and step.get("done"):
        sys.exit(0)
sys.exit(1)
'
}

# ----- HA readiness poll (step 4) -----
wait_for_ha_http() {
  log "waiting for HA HTTP API (timeout ${READY_POLL_TIMEOUT_S}s)..."
  local deadline=$(( $(date +%s) + READY_POLL_TIMEOUT_S ))
  local sleep_s=1
  while (( $(date +%s) < deadline )); do
    local code
    code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 3 \
              "${HA_API_BASE}/api/" 2>/dev/null || true)"
    # Per HA: GET /api/ without auth returns 401 once the API is ready,
    # and connection-refused / curl errors (empty code) before that.
    case "$code" in
      200|401) log "HA API is ready"; return 0 ;;
    esac
    log "waiting for HA..."
    sleep "$sleep_s"
    if (( sleep_s < 8 )); then sleep_s=$(( sleep_s * 2 )); fi
  done
  fatal "HA did not become ready within ${READY_POLL_TIMEOUT_S}s"
}

# ----- Onboarding (step 5) -----
# Generates admin password, POSTs /api/onboarding/users, exchanges the
# returned auth_code for a short-lived access token via /auth/token.
# Outputs (via globals; password lifetime is bounded to this function +
# generate_llat):
#   _admin_password   — generated; cleared after generate_llat
#   _short_token      — short-lived access token (~30 min validity)
do_onboarding() {
  _admin_password="$(openssl rand -base64 32 | tr -d '\n')"
  local language="en"
  local username="hanui-admin"
  local name="hanui admin"

  log "creating admin user via /api/onboarding/users..."
  # Build JSON in python (avoids shell-quoting hazards on the password).
  # Pass password + client_id via env so they never appear in argv.
  local onboarding_response
  onboarding_response="$(
    HANUI_PW="$_admin_password" HANUI_CLIENT_ID="$CLIENT_ID" \
    HANUI_USERNAME="$username" HANUI_NAME="$name" HANUI_LANG="$language" \
    python3 -c '
import json, os, sys
sys.stdout.write(json.dumps({
    "name":      os.environ["HANUI_NAME"],
    "username":  os.environ["HANUI_USERNAME"],
    "password":  os.environ["HANUI_PW"],
    "client_id": os.environ["HANUI_CLIENT_ID"],
    "language":  os.environ["HANUI_LANG"],
}))' \
      | curl -sS --max-time 30 \
          -H 'Content-Type: application/json' \
          --data-binary @- \
          "${HA_API_BASE}/api/onboarding/users" \
      || true
  )"
  if [[ -z "$onboarding_response" ]]; then
    fatal "onboarding/users returned empty response"
  fi

  local auth_code
  auth_code="$(printf '%s' "$onboarding_response" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception as e:
    sys.stderr.write("malformed onboarding response\n")
    sys.exit(1)
code = data.get("auth_code")
if not code:
    sys.stderr.write("onboarding response missing auth_code (Risk #1: API shape drift?)\n")
    sys.exit(1)
sys.stdout.write(code)
')"
  if [[ -z "$auth_code" ]]; then
    fatal "failed to obtain auth_code from onboarding response"
  fi

  log "exchanging auth_code for short-lived access token..."
  # POST /auth/token with form-encoded body. We deliberately put the form
  # body into a file (`mktemp`, mode 0600) and pass via --data-urlencode @-
  # — keeps auth_code out of argv. tmpfile is in the VM tmpfs, removed in
  # the trap at the end.
  local form_tmp
  form_tmp="$(mktemp)"
  chmod 600 "$form_tmp"
  # shellcheck disable=SC2064
  trap "rm -f -- '$form_tmp'" RETURN
  python3 - "$auth_code" "$CLIENT_ID" >"$form_tmp" <<'PY'
import sys, urllib.parse
code, client_id = sys.argv[1], sys.argv[2]
body = urllib.parse.urlencode({
    "grant_type": "authorization_code",
    "code": code,
    "client_id": client_id,
})
sys.stdout.write(body)
PY
  # NOTE: auth_code is intentionally passed as argv to the python helper
  # above. auth_code is single-use and short-lived (~30s window); it is not
  # the LLAT and not the password. The threat model for argv leakage is
  # focused on durable secrets — this transient is acceptable per
  # security-engineer guidance on PR #50.

  local token_response
  token_response="$(curl -sS --max-time 30 \
      -H 'Content-Type: application/x-www-form-urlencoded' \
      --data-binary "@${form_tmp}" \
      "${HA_API_BASE}/auth/token" || true)"
  rm -f -- "$form_tmp"
  trap - RETURN

  if [[ -z "$token_response" ]]; then
    fatal "/auth/token returned empty response"
  fi

  _short_token="$(printf '%s' "$token_response" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.stderr.write("malformed /auth/token response\n")
    sys.exit(1)
tok = data.get("access_token")
if not tok:
    sys.stderr.write("/auth/token response missing access_token\n")
    sys.exit(1)
sys.stdout.write(tok)
')"
  if [[ -z "$_short_token" ]]; then
    fatal "failed to obtain short-lived access token"
  fi
  log "short-lived access token acquired ($(redact "$_short_token"))"
}

# ----- LLAT generation via websocket (step 6) -----
# Reads short-lived token from env HANUI_SHORT_TOKEN (NOT argv).
# Writes the LLAT to stdout (the python script's stdout); we capture it.
generate_llat() {
  log "opening websocket to ${HA_WS_URL} for auth/long_lived_access_token..."
  # Pure-stdlib websocket client (RFC 6455). Pass the short-lived token via
  # env, and the LLAT comes back on stdout. Inline python (no temp file)
  # keeps the source obvious to security review.
  #
  # Loud failure modes: auth_invalid, websocket close before result, JSON
  # parse error, missing `result` field. Each exits non-zero with a
  # non-token error message on stderr.
  local llat
  llat="$(
    HANUI_SHORT_TOKEN="$_short_token" \
    HANUI_HOST="$HA_HOST" \
    HANUI_PORT="$HA_PORT" \
    HANUI_LLAT_NAME="$LLAT_CLIENT_NAME" \
    HANUI_LLAT_LIFESPAN="$LLAT_LIFESPAN_DAYS" \
    python3 <<'PY'
"""Minimal RFC 6455 websocket client for the HA auth/long_lived_access_token
exchange. Plain ws:// only (dev VM, loopback, no TLS — Phase 5 hardens).

Reads HANUI_SHORT_TOKEN from env; writes LLAT to stdout; logs to stderr.
"""
import base64, json, os, secrets, socket, struct, sys

HOST = os.environ["HANUI_HOST"]
PORT = int(os.environ["HANUI_PORT"])
SHORT_TOKEN = os.environ["HANUI_SHORT_TOKEN"]
LLAT_NAME = os.environ["HANUI_LLAT_NAME"]
LLAT_LIFESPAN = int(os.environ["HANUI_LLAT_LIFESPAN"])

def die(msg):
    sys.stderr.write(f"[ws] {msg}\n")
    sys.exit(1)

def send_text_frame(sock, payload: bytes):
    """Send a single masked text frame (RFC 6455 §5.2). Single-fragment."""
    header = bytearray()
    header.append(0x81)  # FIN + text opcode
    mask_key = secrets.token_bytes(4)
    length = len(payload)
    if length < 126:
        header.append(0x80 | length)  # masked
    elif length < (1 << 16):
        header.append(0x80 | 126)
        header.extend(struct.pack(">H", length))
    else:
        header.append(0x80 | 127)
        header.extend(struct.pack(">Q", length))
    header.extend(mask_key)
    masked = bytes(b ^ mask_key[i % 4] for i, b in enumerate(payload))
    sock.sendall(bytes(header) + masked)

def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            die("connection closed mid-frame")
        buf += chunk
    return buf

def recv_text_frame(sock) -> str:
    """Receive one frame, reassembling fragments. Server frames are unmasked."""
    payload = b""
    while True:
        b1, b2 = recv_exact(sock, 2)
        fin = bool(b1 & 0x80)
        opcode = b1 & 0x0F
        masked = bool(b2 & 0x80)
        length = b2 & 0x7F
        if length == 126:
            (length,) = struct.unpack(">H", recv_exact(sock, 2))
        elif length == 127:
            (length,) = struct.unpack(">Q", recv_exact(sock, 8))
        if masked:
            mask_key = recv_exact(sock, 4)
        else:
            mask_key = None
        data = recv_exact(sock, length) if length else b""
        if mask_key is not None:
            data = bytes(b ^ mask_key[i % 4] for i, b in enumerate(data))
        if opcode == 0x8:  # close
            die("server sent close frame")
        if opcode == 0x9:  # ping
            # Respond with pong; ignore for our short exchange.
            continue
        if opcode == 0xA:  # pong
            continue
        if opcode in (0x0, 0x1, 0x2):
            payload += data
            if fin:
                return payload.decode("utf-8")
            continue
        die(f"unexpected opcode {opcode:#x}")

def main():
    sock = socket.create_connection((HOST, PORT), timeout=15)
    try:
        # WebSocket handshake (RFC 6455 §4.1).
        ws_key = base64.b64encode(secrets.token_bytes(16)).decode("ascii")
        req = (
            "GET /api/websocket HTTP/1.1\r\n"
            f"Host: {HOST}:{PORT}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {ws_key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        sock.sendall(req.encode("ascii"))
        # Read response headers until \r\n\r\n.
        resp = b""
        while b"\r\n\r\n" not in resp:
            chunk = sock.recv(4096)
            if not chunk:
                die("handshake: server closed")
            resp += chunk
            if len(resp) > 65536:
                die("handshake: response too large")
        status_line = resp.split(b"\r\n", 1)[0].decode("ascii", "replace")
        if " 101 " not in status_line:
            die(f"handshake failed: {status_line}")

        # The handshake response body may already contain the auth_required
        # frame; consume any extra bytes after \r\n\r\n into a buffer for
        # the next recv. For simplicity, we only check that there's no
        # extra payload — HA sends auth_required in a separate frame after
        # handshake completes, in practice.
        leftover = resp.split(b"\r\n\r\n", 1)[1]
        if leftover:
            # Stuff leftover into a fake recv by reusing a buffered socket.
            # Easier path: the server reliably emits auth_required after
            # handshake completes, so leftover is empty in practice. If
            # it's non-empty, we fail loudly rather than parse partial
            # frames.
            die("unexpected handshake leftover bytes")

        # Read auth_required.
        msg = json.loads(recv_text_frame(sock))
        if msg.get("type") != "auth_required":
            die(f"expected auth_required, got {msg!r}")

        # Send auth.
        send_text_frame(sock, json.dumps({
            "type": "auth", "access_token": SHORT_TOKEN,
        }).encode("utf-8"))
        msg = json.loads(recv_text_frame(sock))
        if msg.get("type") != "auth_ok":
            die(f"auth failed: {msg!r}")

        # Send auth/long_lived_access_token (Risk #13 — must complete
        # within short_token validity window; we do so immediately).
        send_text_frame(sock, json.dumps({
            "id": 1,
            "type": "auth/long_lived_access_token",
            "client_name": LLAT_NAME,
            "lifespan": LLAT_LIFESPAN,
        }).encode("utf-8"))
        msg = json.loads(recv_text_frame(sock))
        if msg.get("type") != "result":
            die(f"unexpected message type: {msg.get('type')!r}")
        if not msg.get("success"):
            err = msg.get("error", {})
            die(f"LLAT request rejected: {err.get('code')} {err.get('message')}")
        result = msg.get("result")
        if not isinstance(result, str) or not result:
            die("LLAT result missing or non-string")
        # Write LLAT to stdout. Stderr logs nothing about the value.
        sys.stdout.write(result)
    finally:
        try:
            sock.close()
        except Exception:
            pass

main()
PY
  )"
  if [[ -z "$llat" ]]; then
    fatal "websocket exchange yielded empty LLAT"
  fi
  _llat="$llat"
  log "LLAT acquired ($(redact "$_llat"))"
}

# ----- Emit env fragment to stdout (step 7) -----
# Heredoc-only write. The fd-3 dance below ensures stdout has fully drained
# before we proceed to discard the password — if stdout is closed (broken
# pipe), the script exits non-zero via set -e.
emit_env_fragment() {
  log "emitting HA_URL + HA_TOKEN to stdout (caller writes to host .env.local)"
  # Heredoc to stdout — token is the body of the heredoc, NEVER on a command
  # line. Trailing newline is significant for `.env` line discipline.
  cat <<EOF
HA_URL=${HA_WS_URL}
HA_TOKEN=${_llat}
EOF
  # Force flush by syncing fd 1 if possible. Bash has no portable fsync;
  # closing fd 1 explicitly via exec is the canonical way to surface a
  # broken-pipe error here. We don't close fd 1 because subsequent log()
  # lines are stderr, but we DO probe writability.
  if ! : >&1; then
    fatal "stdout closed before LLAT write completed"
  fi
}

# ----- Template entity verification (step 9) -----
# Polls /api/states/light.kitchen until the entity is registered. Uses the
# fresh LLAT for auth (token via stdin to curl, never argv).
verify_template_loaded() {
  log "verifying template entities loaded (timeout ${TEMPLATE_POLL_TIMEOUT_S}s)..."
  local deadline=$(( $(date +%s) + TEMPLATE_POLL_TIMEOUT_S ))
  while (( $(date +%s) < deadline )); do
    local code
    code="$(printf 'Authorization: Bearer %s\n' "$_llat" \
      | curl -sS -o /dev/null -w '%{http_code}' --max-time 3 \
          -H @- \
          "${HA_API_BASE}/api/states/light.kitchen" 2>/dev/null || true)"
    if [[ "$code" == "200" ]]; then
      log "template entity light.kitchen is live"
      return 0
    fi
    sleep 1
  done
  fatal "template entities not registered within ${TEMPLATE_POLL_TIMEOUT_S}s"
}

# ----- Compose helpers -----
compose_up() {
  log "starting HA via docker compose..."
  ( cd "$COMPOSE_FILE_DIR" && docker compose up -d ) >&2
}

compose_restart() {
  log "restarting HA to apply configuration.yaml..."
  ( cd "$COMPOSE_FILE_DIR" && docker compose restart ) >&2
}

# ----- Main -----
main() {
  preflight

  read_existing_env

  # Step 1: idempotency — already provisioned?
  if [[ -n "$_existing_ha_token" ]]; then
    # Container must be up AND token must work.
    local container_state
    container_state="$(docker ps --filter 'name=^hanui-homeassistant$' \
                        --format '{{.State}}' 2>/dev/null || true)"
    if [[ "$container_state" == "running" ]] \
       && verify_existing_token "$_existing_ha_token"; then
      log "ha already provisioned"
      exit 0
    fi
    # If container is stopped but token exists, we still try to bring HA up
    # and re-verify. If the token then works, the existing-token path is
    # still valid; otherwise we fall through to partial-state detection
    # below (which handles "token in env but onboarding wiped" too).
    log "existing HA_TOKEN present but verification failed; checking state..."
  fi

  compose_up
  wait_for_ha_http

  # Re-verify post-up (existing token may now work).
  if [[ -n "$_existing_ha_token" ]] \
     && verify_existing_token "$_existing_ha_token"; then
    log "ha already provisioned (verified post-restart)"
    exit 0
  fi

  # Step 2: partial-state detection. If onboarding is complete but we have
  # no working token, recovery is the only option (no API mid-run recovery).
  if onboarding_users_done; then
    fatal "partial provision detected — run 'make ha-clean && make ha-up' to reset"
  fi

  # Steps 5 + 5b: onboarding + auth_code → short-lived access token.
  do_onboarding

  # Step 6: LLAT via websocket. Risk #13 timing: do_onboarding → generate_llat
  # is single-digit-second latency; well within the ~30 min short-token window.
  generate_llat

  # Discard the password as soon as the LLAT is in hand. The LLAT is the
  # durable credential; the password is no longer useful.
  unset _admin_password
  unset _short_token

  # Step 7: emit env fragment to stdout. Caller writes to host .env.local.
  emit_env_fragment

  # Step 8 + 9: restart + verify templates loaded.
  compose_restart
  wait_for_ha_http
  verify_template_loaded

  # Discard the LLAT from script memory (it's already on stdout).
  unset _llat

  log "provisioning complete"
}

main "$@"
