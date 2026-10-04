.PHONY: install web build serve check format test-s3 measure-snapshot measure-resources

install:
	cd web && pnpm install --frozen-lockfile

web:
	cd web && pnpm build

build: web
	cargo build --release --locked

serve: web
	cargo run --locked -- serve

format:
	cargo fmt --all
	cd web && pnpm format

check: web
	cd web && pnpm format:check
	cd web && pnpm lint
	cd web && pnpm check
	cd web && pnpm test
	cargo fmt --all -- --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --all-targets --locked
	cargo build --locked
	cd web && pnpm test:e2e
	cd web && pnpm test:s3

test-s3:
	cd web && pnpm test:s3

# Synthetic local evidence; does not assert product capacity or latency targets.
measure-snapshot: web
	cargo run --release --locked --example resource_spike

measure-resources: web
	cargo build --release --locked --example resource_spike
	cd web && pnpm measure:resources
