locals {
  table_name = "${var.name_prefix}-memories"

  # The vector index definition, kept in one place so the create path and the
  # replace-detection trigger cannot drift apart.
  vector_index = {
    IndexName       = var.vector_index_name
    VectorAttribute = { AttributeName = "embedding" }
    SearchSchema = [
      # The single most consequential line in this stack. Scoping the index by
      # user means a search scans one person's vectors instead of the whole
      # corpus — and since vector search is billed per byte processed, that is
      # what keeps the bill proportional to a user rather than to the dataset.
      # It also buys per-user throughput quotas, because those are enforced per
      # partition key value.
      { AttributeName = "user_id", SearchSchemaElementType = "HASH" },
      { AttributeName = "kind", SearchSchemaElementType = "INLINE_FILTER" },
    ]
    Projection = {
      ProjectionType   = "INCLUDE"
      NonKeyAttributes = ["text", "created_at", "source"]
    }
    Dimensions = var.embedding_dimensions
    # Lower scores mean closer matches. The domain's `Distance` type encodes
    # that direction; changing this function would require changing the one
    # conversion in the DynamoDB adapter.
    DistanceFunction = "COSINE"
  }
}

resource "aws_dynamodb_table" "memories" {
  name = local.table_name
  # Not a preference: vector indexes exist only on on-demand tables. A
  # PROVISIONED table is rejected outright.
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "user_id"
  range_key    = "memory_id"

  attribute {
    name = "user_id"
    type = "S"
  }

  attribute {
    name = "memory_id"
    type = "S"
  }

  # `kind` is deliberately absent here even though the vector index needs it.
  # The provider runs a CustomizeDiff (validateTableAttributes, in
  # internal/service/dynamodb/table.go) that fails the plan with
  # "all attributes must be indexed" for any attribute no key or secondary
  # index references — and the provider cannot see vector indexes at all.
  # terraform_data.vector_index registers it through UpdateTable instead.

  ttl {
    attribute_name = "expires_at"
    enabled        = true
  }

  point_in_time_recovery {
    enabled = true
  }

  # The escape hatch adds `kind` to the table's AttributeDefinitions. Without
  # this, every subsequent plan would see an attribute it did not declare and
  # try to remove it, which would in turn break the vector index.
  lifecycle {
    ignore_changes = [attribute]
  }
}

# ---------------------------------------------------------------------------
# Vector index escape hatch
# ---------------------------------------------------------------------------
# DynamoDB vector search went GA on 2026-08-04 and no infrastructure-as-code
# path exposes it yet:
#
#   * hashicorp/aws `aws_dynamodb_table` has no vector index arguments;
#   * CloudFormation's AWS::DynamoDB::Table has no VectorIndexes property;
#   * `awscc_dynamodb_table` is generated from that CloudFormation schema, so it
#     inherits the same gap.
#
# Everything that *can* be declarative is (the table, its TTL, PITR, tags and
# destroy lifecycle). Only the index itself goes through a provisioner, isolated
# here so it can be deleted wholesale once the provider catches up.
resource "terraform_data" "vector_index" {
  # A destroy-time provisioner may only reference `self`, so every value the
  # delete path needs has to be captured here rather than read from a variable.
  triggers_replace = {
    table  = aws_dynamodb_table.memories.name
    index  = var.vector_index_name
    region = var.region
    config = jsonencode(local.vector_index)
  }

  provisioner "local-exec" {
    command = "${path.module}/scripts/vector-index.sh create"

    environment = {
      TABLE_NAME              = self.triggers_replace.table
      INDEX_NAME              = self.triggers_replace.index
      INDEX_CONFIG            = self.triggers_replace.config
      AWS_REGION              = self.triggers_replace.region
      INLINE_FILTER_ATTRIBUTE = "kind"
    }
  }

  provisioner "local-exec" {
    when    = destroy
    command = "${path.module}/scripts/vector-index.sh delete"

    environment = {
      TABLE_NAME = self.triggers_replace.table
      INDEX_NAME = self.triggers_replace.index
      AWS_REGION = self.triggers_replace.region
    }
  }
}
