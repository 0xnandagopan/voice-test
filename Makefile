.PHONY: check test-db test-media test-browser test-api

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo test --workspace --locked
	npm --prefix web run build
	npm --prefix web run format:check
	node --experimental-strip-types --test --test-isolation=none web/src/audio/*.test.mjs

test-db:
	@test -n "$(TEST_DATABASE_URL)" || (echo 'Set TEST_DATABASE_URL to isolated PostgreSQL'; exit 1)
	cargo test -p v0-app --test access --locked -- --ignored
	cargo test -p v0-app --test workflow --locked -- --ignored
	cargo test -p v0-evidence --test recovery --locked -- --include-ignored

test-media:
	cargo test -p v0-evidence --test media --locked real_opus_decode_validates_channels_bounds_and_reconstruction_without_approval -- --ignored --exact

test-browser:
	npm --prefix web test

test-api:
	cargo build --locked -p v0-app
	npm --prefix web run build
	python3 scripts/test-headless-api.py
