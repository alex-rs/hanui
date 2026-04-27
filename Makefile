.PHONY: dev hooks test lint check vm-docker ha-up ha-down ha-clean ha-token

# === Phase 2.5 HA target configuration ===
#
# Path INSIDE the dev VM where the hanui repo is checked out. The HA
# provisioning script and docker-compose.yml live under
# $(VM_REPO_PATH)/ops/ha/. Override with `make ha-up VM_REPO_PATH=/some/path`
# if your VM checkout lives elsewhere. The default `~/hanui` matches the
# `dev` user's home directory inside the cloud-init-provisioned VM (see
# vm/cloud-init/user-data).
VM_REPO_PATH ?= ~/hanui

dev:
	@echo "Starting hanui dev stack..."
	docker compose up -d

hooks:
	lefthook install




test:
	cargo test

lint:
	cargo clippy -- -D warnings
	cargo fmt -- --check

check: lint test
	@echo "All checks passed."

# One-shot Docker install for VMs that booted before TASK-056.
# Cloud-init (vm/cloud-init/user-data) installs the same `docker-ce`
# family on freshly provisioned VMs via /usr/local/bin/install-docker.sh;
# this target ships the SAME install sequence over SSH for older VMs
# whose cloud-init ran before TASK-056 and lacks that script.
#
# Why `docker-ce` and not Debian's `docker.io`: `docker-compose-plugin`
# is not in Debian 12 stable apt — it ships only in Docker's upstream
# repo. `docker-ce` (Docker's package) provides /usr/bin/docker and
# REPLACES `docker.io` (they conflict on the same path), so we install
# the full `docker-ce` family from the upstream repo.
#
# Security: the recipe pins Docker's published GPG-key fingerprint
# (9DC8 5822 9FC7 DD38 854A E2D8 8D81 803C 0EBF CD88, per
# https://docs.docker.com/engine/install/debian/) and refuses to install
# if it mismatches. This is the load-bearing control against a
# compromised Docker apt repo (security-engineer review).
#
# Idempotent: skips key-fetch if /etc/apt/keyrings/docker.asc exists,
# skips sources-add if /etc/apt/sources.list.d/docker.list exists,
# `apt-get install` is a no-op for already-installed packages.
#
# Implementation note: the install script is defined as a Make variable
# (`define ... endef`) and exported to the environment, then piped to
# `sudo bash -s` over SSH. This keeps the heredoc-style multi-line
# script working without `.ONESHELL:` (which would change semantics for
# every other recipe in this file).
define VM_DOCKER_INSTALL_SCRIPT
set -euo pipefail
DOCKER_GPG_FINGERPRINT='9DC8 5822 9FC7 DD38 854A E2D8 8D81 803C 0EBF CD88'
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends ca-certificates curl gnupg
install -m 0755 -d /etc/apt/keyrings
if [ ! -f /etc/apt/keyrings/docker.asc ]; then
  curl -fsSL https://download.docker.com/linux/debian/gpg \
    -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
fi
actual_fpr=$$(gpg --dry-run --quiet --no-keyring \
  --import --import-options import-show \
  /etc/apt/keyrings/docker.asc 2>/dev/null \
  | awk '/^ +[0-9A-F ]+$$/ { print; exit }' \
  | tr -d ' ')
expected_fpr=$$(echo "$$DOCKER_GPG_FINGERPRINT" | tr -d ' ')
if [ "$$actual_fpr" != "$$expected_fpr" ]; then
  echo "ERROR: Docker GPG key fingerprint mismatch." >&2
  echo "       expected: $$expected_fpr" >&2
  echo "       actual:   $$actual_fpr" >&2
  echo "       refusing to install -- possible apt repo compromise." >&2
  exit 1
fi
if [ ! -f /etc/apt/sources.list.d/docker.list ]; then
  arch=$$(dpkg --print-architecture)
  codename=$$(. /etc/os-release && echo "$$VERSION_CODENAME")
  echo "deb [arch=$${arch} signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian $${codename} stable" \
    > /etc/apt/sources.list.d/docker.list
fi
apt-get update -qq
apt-get install -y --no-install-recommends \
  docker-ce docker-ce-cli containerd.io \
  docker-buildx-plugin docker-compose-plugin
systemctl enable --now docker
endef
export VM_DOCKER_INSTALL_SCRIPT

vm-docker:
	@echo "Installing docker-ce + compose v2 in the dev VM (via Docker upstream apt repo)..."
	@printf '%s\n' "$$VM_DOCKER_INSTALL_SCRIPT" | ./ssh-vm.sh -- 'sudo bash -s'

# === Phase 2.5: Local HA target (TASK-053) ===
#
# All ha-* recipes SSH into the dev VM via ./ssh-vm.sh. The provisioning
# script and compose file live inside the VM at $(VM_REPO_PATH)/ops/ha/;
# the host wrapper here is responsible for (a) reachability pre-checks,
# (b) shipping existing HA_* env lines to provision-ha.sh stdin so it can
# detect already-provisioned state without leaking the token to argv, and
# (c) merging the script's stdout fragment back into the host's .env.local
# atomically.
#
# Token handling discipline:
#   - The token NEVER appears as a shell argument (no echo "$$TOKEN", no
#     positional args). It transits SSH stdout into a 0600 temp file and is
#     `mv`'d atomically into .env.local -- never visible to `ps`.
#   - .env.local is rewritten via mktemp + mv (atomic on the same fs) so a
#     concurrent reader sees either the old contents or the new contents,
#     never a partial write. Temp files use the `.env.*.local` naming pattern
#     so they fall under the existing gitignore rule on line 9 -- this
#     guarantees a stray temp file (kill -9 leaving the trap unfired) cannot
#     be accidentally `git add`ed.
#   - Recipes never `cat .env.local`; existence-only assertions only.
#
# Risks (per docs/plans/2026-04-27-phase-2.5-local-ha-target.md):
#   #7  VM not running    -> reachability pre-check below
#   #10 ha-clean while connected -> 2-second warning before destructive op
#   #11 SSH key missing/rotated  -> reachability pre-check fails loudly
#
# Self-test: `make -n ha-up` (dry-run) is the cheapest way to inspect the
# expanded recipe without invoking SSH.

# Reachability pre-check macro. Echos a human-readable error and exits 1
# if the VM SSH port is unreachable or auth fails. Used by every ha-*
# target. Must be the first action in each recipe to fail fast.
define HA_SSH_REACHABILITY_CHECK
	if ! ./ssh-vm.sh -o ConnectTimeout=5 -o BatchMode=yes -- true >/dev/null 2>&1; then \
	  echo "ERROR: VM not reachable over SSH." >&2; \
	  echo "       Run \`./run-vm.sh\` to boot the dev VM, or check vm/keys/id_ed25519 permissions." >&2; \
	  exit 1; \
	fi
endef

ha-up:
	@$(HA_SSH_REACHABILITY_CHECK)
	@echo "Provisioning HA in VM (this may take ~2 minutes on first run)..."
	@set -eu; umask 077; \
	frag_tmp=$$(mktemp -p . .env.frag.XXXXXX.local); \
	merged_tmp=$$(mktemp -p . .env.merged.XXXXXX.local); \
	trap 'rm -f -- "$$frag_tmp" "$$merged_tmp"' EXIT; \
	chmod 600 "$$frag_tmp" "$$merged_tmp"; \
	{ \
	  if [ -f .env.local ]; then \
	    grep -E '^(HA_URL|HA_TOKEN)=' .env.local || true; \
	  fi; \
	} | ./ssh-vm.sh -- 'bash $(VM_REPO_PATH)/ops/ha/provision-ha.sh' > "$$frag_tmp"; \
	if [ ! -s "$$frag_tmp" ]; then \
	  echo "HA already provisioned (no token write needed)."; \
	  exit 0; \
	fi; \
	if ! grep -q '^HA_URL=' "$$frag_tmp" || ! grep -q '^HA_TOKEN=' "$$frag_tmp"; then \
	  echo "ERROR: provision-ha.sh stdout did not contain HA_URL= and HA_TOKEN= lines." >&2; \
	  exit 1; \
	fi; \
	if [ -f .env.local ]; then \
	  grep -vE '^(HA_URL|HA_TOKEN)=' .env.local > "$$merged_tmp" || true; \
	fi; \
	cat "$$frag_tmp" >> "$$merged_tmp"; \
	chmod 600 "$$merged_tmp"; \
	mv -- "$$merged_tmp" .env.local; \
	echo "HA provisioned. .env.local updated (HA_URL + HA_TOKEN written; token redacted from output)."

ha-down:
	@$(HA_SSH_REACHABILITY_CHECK)
	@echo "Stopping HA container in VM (volume preserved)..."
	./ssh-vm.sh -- 'docker compose -f $(VM_REPO_PATH)/ops/ha/docker-compose.yml down'

ha-clean:
	@echo "WARNING: stop \`cargo run\` before ha-clean to avoid connection errors (Risk #10)."
	@echo "         Removing HA container, ha_data volume, AND HA_URL/HA_TOKEN from .env.local."
	@sleep 2
	@$(HA_SSH_REACHABILITY_CHECK)
	./ssh-vm.sh -- 'docker compose -f $(VM_REPO_PATH)/ops/ha/docker-compose.yml down -v'
	@set -eu; umask 077; \
	if [ -f .env.local ]; then \
	  merged_tmp=$$(mktemp -p . .env.scrub.XXXXXX.local); \
	  trap 'rm -f -- "$$merged_tmp"' EXIT; \
	  chmod 600 "$$merged_tmp"; \
	  grep -vE '^(HA_URL|HA_TOKEN)=' .env.local > "$$merged_tmp" || true; \
	  mv -- "$$merged_tmp" .env.local; \
	  echo "HA_URL/HA_TOKEN removed from .env.local."; \
	else \
	  echo ".env.local not present; nothing to scrub."; \
	fi

# Re-issue the long-lived access token by tearing down the HA instance and
# re-provisioning. This is the simplest in-scope implementation: it stays
# within the existing provision-ha.sh contract (no new flag, no helper
# script extracted), at the cost of resetting all entity state. For a
# dev VM that is acceptable; entity state is recreated by HA on next boot
# from configuration.yaml.
#
# Security note (escalate_to: security-engineer): this target rewrites the
# HA_TOKEN boundary in .env.local. The new token transits the same SSH
# stdout path as ha-up and is never echoed. Runs ha-clean first to revoke
# the old token by destroying the HA instance that issued it.
ha-token:
	@echo "Re-issuing HA token via ha-clean + ha-up (revokes old token; resets entity state)..."
	@$(MAKE) ha-clean
	@$(MAKE) ha-up
