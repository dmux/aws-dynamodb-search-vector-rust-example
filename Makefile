SHELL := /bin/bash
.DEFAULT_GOAL := help

TERRAFORM_DIR := terraform
LAMBDA_CRATE  := agent-memory-lambda
MCP_CRATE     := agent-memory-mcp
LAMBDA_ZIP    := target/lambda/$(LAMBDA_CRATE)/bootstrap.zip
MCP_BIN       := target/release/$(MCP_CRATE)

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

# --- Preflight -------------------------------------------------------------

.PHONY: preflight
preflight: ## Check the tools needed to cross-compile the Lambda
	@missing=0; \
	if ! command -v cargo-lambda >/dev/null 2>&1; then \
		echo "MISSING: cargo-lambda   install with: cargo install cargo-lambda"; \
		missing=1; \
	fi; \
	if ! command -v zig >/dev/null 2>&1 && ! python3 -c 'import ziglang' >/dev/null 2>&1; then \
		echo "MISSING: zig            install with: pip3 install ziglang"; \
		echo "  cargo-lambda cross-compiles through cargo-zigbuild. This is not optional:"; \
		echo "  a natively linked binary picks up this machine's glibc, which is newer than"; \
		echo "  the one on Amazon Linux 2023, and the function dies at runtime with a GLIBC"; \
		echo "  version error. zigbuild pins the target glibc instead."; \
		missing=1; \
	fi; \
	if [ "$$missing" -ne 0 ]; then exit 1; fi; \
	echo "preflight ok"

# --- Build -----------------------------------------------------------------

.PHONY: build
build: preflight ## Build the Lambda zip for arm64 (Graviton)
	cargo lambda build --release --arm64 --output-format zip -p $(LAMBDA_CRATE)
	@echo "artifact: $(LAMBDA_ZIP)"

.PHONY: build-mcp
build-mcp: ## Build the local MCP server binary
	cargo build --release -p $(MCP_CRATE)
	@echo "artifact: $(MCP_BIN)"

# --- Quality ---------------------------------------------------------------

.PHONY: fmt
fmt: ## Check formatting
	cargo fmt --all -- --check

.PHONY: fmt-fix
fmt-fix: ## Apply formatting
	cargo fmt --all

.PHONY: lint
lint: ## Run clippy with warnings denied
	cargo clippy --workspace --all-targets --all-features -- -D warnings

.PHONY: test
test: ## Run unit tests (no AWS calls, no cost)
	cargo test --workspace --all-features

.PHONY: test-it
test-it: ## Run integration tests against real AWS (creates resources, costs money)
	@if [ -z "$$AWS_REGION" ]; then echo "set AWS_REGION first"; exit 1; fi
	DDB_VECTOR_IT=1 cargo test --workspace --all-features -- --ignored --nocapture --test-threads=1

.PHONY: check
check: fmt lint test ## Format check, lint and test

# --- Deploy ----------------------------------------------------------------

.PHONY: deploy
deploy: build ## Apply the Terraform stack
	cd $(TERRAFORM_DIR) && terraform init -input=false && terraform apply

.PHONY: plan
plan: ## Show the Terraform plan
	cd $(TERRAFORM_DIR) && terraform init -input=false && terraform plan

.PHONY: destroy
destroy: ## Destroy the Terraform stack
	cd $(TERRAFORM_DIR) && terraform destroy

.PHONY: smoke
smoke: ## Store and recall one memory through the deployed API
	./scripts/smoke.sh

.PHONY: mcp-config
mcp-config: build-mcp ## Print the .mcp.json block wired to the deployed API
	@api=$$(cd $(TERRAFORM_DIR) && terraform output -raw api_endpoint); \
	region=$$(cd $(TERRAFORM_DIR) && terraform output -raw region); \
	printf '{\n  "mcpServers": {\n    "agent-memory": {\n      "command": "%s",\n      "env": {\n        "AGENT_MEMORY_API": "%s",\n        "AWS_REGION": "%s"\n      }\n    }\n  }\n}\n' \
		"$$(pwd)/$(MCP_BIN)" "$$api" "$$region"
