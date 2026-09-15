# The local gate. `make check` runs exactly what CI runs (MASTER_PLAN.md §8.1),
# so a green local run means a green PR rather than a surprise.
#
# Anything added to .github/workflows/ci.yml should be reachable from here.

CARGO ?= cargo

.PHONY: help check fmt clippy test msrv docs deny build clean

help: ## Show available targets
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

check: fmt clippy test docs ## Run every CI gate locally

fmt: ## Verify formatting
	$(CARGO) fmt --all --check

clippy: ## Lint with warnings denied
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

test: ## Run the test suite
	$(CARGO) test --workspace --all-targets

msrv: ## Verify the declared MSRV promise (ADR-001) on the pinned toolchain
	@v=$$(grep -m1 '^rust-version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/'); \
	 echo "Declared MSRV: $$v"; \
	 $(CARGO) +$$v check --workspace --all-targets

docs: ## Build rustdoc with warnings denied
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps --document-private-items

deny: ## Check licenses and advisories (SECURITY.md §10)
	$(CARGO) deny check

build: ## Release build
	$(CARGO) build --release --workspace

clean: ## Remove build artifacts
	$(CARGO) clean
