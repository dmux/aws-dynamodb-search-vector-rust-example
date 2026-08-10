#!/usr/bin/env bash
#
# Store and recall one memory through the deployed API.
#
# The actual work is done by the `smoke` example in agent-memory-client, so it
# exercises the same SigV4 signing code the MCP server uses rather than a
# parallel implementation that could drift.

set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly TERRAFORM_DIR="${ROOT}/terraform"

if [[ ! -d "${TERRAFORM_DIR}/.terraform" ]]; then
  echo "terraform has not been initialised; run 'make deploy' first" >&2
  exit 1
fi

AGENT_MEMORY_API="$(terraform -chdir="$TERRAFORM_DIR" output -raw api_endpoint)"
AWS_REGION="$(terraform -chdir="$TERRAFORM_DIR" output -raw region)"
export AGENT_MEMORY_API AWS_REGION

echo "endpoint: ${AGENT_MEMORY_API}"
echo "region:   ${AWS_REGION}"
echo

exec cargo run --quiet --release -p agent-memory-client --example smoke
