.PHONY: test lint fixtures

test:
	cargo test --workspace

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

# Recapture golden fixtures from the reference Go daemon (operator machine only; see harness/capture/)
fixtures:
	harness/capture/capture.sh
