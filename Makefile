.PHONY: install web build serve check format test-s3

install:
	pnpm --dir web install --frozen-lockfile

web:
	pnpm --dir web build

build: web
	cargo build --release --locked

serve: web
	cargo run --locked -- serve

format:
	cargo fmt --all
	pnpm --dir web format

check: web
	pnpm --dir web format:check
	pnpm --dir web lint
	pnpm --dir web check
	pnpm --dir web test
	cargo fmt --all -- --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --all-targets --locked
	cargo build --locked
	pnpm --dir web test:e2e
	pnpm --dir web test:s3

test-s3:
	pnpm --dir web test:s3
