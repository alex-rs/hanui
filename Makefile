.PHONY: dev hooks test lint check

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
