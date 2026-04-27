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

# One-shot Docker install for VMs that booted before TASK-050.
# Cloud-init now installs docker.io + docker-compose-plugin on freshly
# provisioned VMs; this target covers existing VMs so they don't require
# a full rebuild. Idempotent: apt-get install -y on already-installed
# packages is a no-op aside from the index refresh.
# See docs/plans/2026-04-27-phase-2.5-local-ha-target.md (Risk #14) for
# the cloud-init-vs-make-vm-docker drift acknowledgement.
vm-docker:
	@echo "Installing docker.io + docker-compose-plugin in the dev VM..."
	./ssh-vm.sh -- 'sudo apt-get update -qq && sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends docker.io docker-compose-plugin'

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
