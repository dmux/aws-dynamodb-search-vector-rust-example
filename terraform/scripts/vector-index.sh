#!/usr/bin/env bash
#
# Create or delete a DynamoDB vector index.
#
# This exists because no Terraform provider and no CloudFormation resource can
# express a vector index yet. It is called from terraform_data.vector_index and
# is written to be idempotent, so a re-apply after a partial failure converges
# instead of erroring.
#
# Required environment:
#   TABLE_NAME, INDEX_NAME, AWS_REGION
#   INDEX_CONFIG, INLINE_FILTER_ATTRIBUTE  (create only)

set -euo pipefail

readonly ACTION="${1:?usage: vector-index.sh create|delete}"
readonly TABLE_NAME="${TABLE_NAME:?TABLE_NAME is required}"
readonly INDEX_NAME="${INDEX_NAME:?INDEX_NAME is required}"

# Index creation, and especially backfill, is measured in minutes.
readonly READY_TIMEOUT_SECONDS=900
readonly POLL_INTERVAL_SECONDS=10

log() { printf '[vector-index] %s\n' "$*" >&2; }

require_tools() {
  for tool in aws jq; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      log "ERROR: $tool is required but not installed"
      exit 1
    fi
  done
}

# Print the index description, or nothing when the index does not exist.
describe_index() {
  aws dynamodb describe-table \
    --table-name "$TABLE_NAME" \
    --query "Table.VectorIndexes[?IndexName=='${INDEX_NAME}'] | [0]" \
    --output json 2>/dev/null || echo 'null'
}

# Wait until the index will actually answer a search.
#
# Two conditions, not one. `aws dynamodb wait table-exists` is the obvious
# choice and it is wrong: it returns once TableStatus is ACTIVE, which happens
# while the index is still CREATING. And an index added to an existing table
# reports ACTIVE *before* backfill finishes, during which SearchVectors errors
# out. There is no AWS waiter for either condition.
wait_until_searchable() {
  local deadline=$((SECONDS + READY_TIMEOUT_SECONDS))

  while ((SECONDS < deadline)); do
    local index status backfilling
    index="$(describe_index)"

    if [[ "$index" == "null" || -z "$index" ]]; then
      log "index not reported yet; waiting"
    else
      status="$(jq -r '.IndexStatus // "UNKNOWN"' <<<"$index")"
      # Absent means "not backfilling": DescribeTable omits the flag for an
      # index created as part of CreateTable.
      backfilling="$(jq -r '.Backfilling // false' <<<"$index")"

      if [[ "$status" == "ACTIVE" && "$backfilling" == "false" ]]; then
        log "index ${INDEX_NAME} is searchable"
        return 0
      fi
      log "status=${status} backfilling=${backfilling}; waiting"
    fi

    sleep "$POLL_INTERVAL_SECONDS"
  done

  log "ERROR: ${INDEX_NAME} was not searchable within ${READY_TIMEOUT_SECONDS}s"
  return 1
}

wait_until_gone() {
  local deadline=$((SECONDS + READY_TIMEOUT_SECONDS))

  while ((SECONDS < deadline)); do
    if [[ "$(describe_index)" == "null" ]]; then
      log "index ${INDEX_NAME} removed"
      return 0
    fi
    log "index still present; waiting"
    sleep "$POLL_INTERVAL_SECONDS"
  done

  log "ERROR: ${INDEX_NAME} still present after ${READY_TIMEOUT_SECONDS}s"
  return 1
}

create_index() {
  local config="${INDEX_CONFIG:?INDEX_CONFIG is required}"
  local filter_attribute="${INLINE_FILTER_ATTRIBUTE:?INLINE_FILTER_ATTRIBUTE is required}"

  local existing
  existing="$(describe_index)"
  if [[ "$existing" != "null" && -n "$existing" ]]; then
    log "index ${INDEX_NAME} already exists; converging on its readiness"
    wait_until_searchable
    return 0
  fi

  # AttributeDefinitions travels with the update because the inline filter
  # attribute is referenced only by the vector index SearchSchema, and so was
  # never declared on the table. UpdateTable documents this parameter in terms
  # of global secondary indexes; the integration test
  # `adding_a_vector_index_can_register_its_search_schema_attribute` is what
  # verifies it also holds for a vector index.
  local existing_attrs
  existing_attrs="$(aws dynamodb describe-table --table-name "$TABLE_NAME" --query "Table.AttributeDefinitions" --output json)"
  local merged_attrs
  merged_attrs="$(jq -c --arg attr "$filter_attribute" '
    . + [{"AttributeName": $attr, "AttributeType": "S"}] | unique_by(.AttributeName)
  ' <<<"$existing_attrs")"

  log "creating vector index ${INDEX_NAME} on ${TABLE_NAME}"
  aws dynamodb update-table \
    --table-name "$TABLE_NAME" \
    --attribute-definitions "$merged_attrs" \
    --vector-index-updates "$(jq -n --argjson index "$config" '[{Create: $index}]')" \
    >/dev/null

  wait_until_searchable
}

delete_index() {
  if [[ "$(describe_index)" == "null" ]]; then
    log "index ${INDEX_NAME} is already gone"
    return 0
  fi

  # This runs before the table is destroyed, because terraform_data depends on
  # the table. That ordering also avoids DeleteTable failing with
  # ResourceInUseException: "Cannot delete table while indexes are being
  # created, updated, or deleted."
  log "deleting vector index ${INDEX_NAME}"
  aws dynamodb update-table \
    --table-name "$TABLE_NAME" \
    --vector-index-updates "$(jq -n --arg name "$INDEX_NAME" '[{Delete: {IndexName: $name}}]')" \
    >/dev/null

  wait_until_gone
}

require_tools

case "$ACTION" in
  create) create_index ;;
  delete) delete_index ;;
  *)
    log "ERROR: unknown action '${ACTION}' (expected create or delete)"
    exit 1
    ;;
esac
