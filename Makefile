.PHONY: dev hooks test lint check vm-docker

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
