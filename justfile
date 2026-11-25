# Default recipe when running `just` without arguments
default: check

# Run all checks
check:
	typos --sort
	cargo sort --check --check-format --grouped --workspace
	cargo deny check
	cargo +nightly fmt --check
	cargo clippy
	cargo test

# Individual check recipes
typos:
	typos --sort

sort:
	cargo sort --check --check-format --grouped --workspace

deny:
	cargo deny check

fmt:
	cargo +nightly fmt --check

clippy:
	cargo clippy

test:
	cargo test

# Fix recipes (auto-fix where possible)
fix-fmt:
	cargo +nightly fmt

fix-clippy:
	cargo clippy --fix --allow-dirty --allow-staged

fix-sort:
	cargo sort --grouped --workspace

fix-typos:
	typos --write-changes

# Run all fixes (sequential: typos -> sort -> clippy -> fmt)
fix:
	typos --write-changes
	cargo sort --grouped --workspace
	cargo clippy --fix --allow-dirty --allow-staged
	cargo +nightly fmt

# Build recipes
build:
	cargo build

build-release:
	cargo build --release

# Clean
clean:
	cargo clean

# Update dependencies
update:
	cargo update

# Generate docs
docs:
	cargo doc --no-deps
