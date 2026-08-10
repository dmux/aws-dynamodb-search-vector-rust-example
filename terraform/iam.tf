locals {
  vector_index_arn = "${aws_dynamodb_table.memories.arn}/index/${var.vector_index_name}"
}

data "aws_iam_policy_document" "lambda_assume_role" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "lambda" {
  name               = "${var.name_prefix}-lambda"
  assume_role_policy = data.aws_iam_policy_document.lambda_assume_role.json
}

resource "aws_iam_role_policy_attachment" "lambda_basic_execution" {
  role       = aws_iam_role.lambda.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

data "aws_iam_policy_document" "lambda" {
  # `dynamodb:SearchVectors` is a new action introduced with vector search, and
  # the resource is the *index* ARN rather than the table's. Pre-existing
  # read policies do not grant it, so it has to be stated explicitly.
  statement {
    sid       = "SearchVectorIndex"
    actions   = ["dynamodb:SearchVectors"]
    resources = [local.vector_index_arn]
  }

  statement {
    sid = "ReadWriteMemories"
    actions = [
      "dynamodb:PutItem",
      "dynamodb:GetItem",
      "dynamodb:DeleteItem",
      "dynamodb:BatchWriteItem",
    ]
    resources = [aws_dynamodb_table.memories.arn]
  }

  # Scoped to the one model actually used, rather than every foundation model.
  statement {
    sid       = "InvokeEmbeddingModel"
    actions   = ["bedrock:InvokeModel"]
    resources = ["arn:aws:bedrock:${var.region}::foundation-model/${var.embedding_model_id}"]
  }
}

resource "aws_iam_policy" "lambda" {
  name   = "${var.name_prefix}-lambda"
  policy = data.aws_iam_policy_document.lambda.json
}

resource "aws_iam_role_policy_attachment" "lambda" {
  role       = aws_iam_role.lambda.name
  policy_arn = aws_iam_policy.lambda.arn
}

# ---------------------------------------------------------------------------
# Caller policies
# ---------------------------------------------------------------------------
# Attach one of these to whoever runs the MCP server. Splitting the API into
# four routes rather than one is what makes the read-only variant expressible:
# with a single route, "may search" and "may delete" could not be separated.

data "aws_iam_policy_document" "caller_read_write" {
  statement {
    sid       = "FullMemoryAccess"
    actions   = ["execute-api:Invoke"]
    resources = ["${aws_apigatewayv2_api.memory.execution_arn}/*/*/memories*"]
  }
}

data "aws_iam_policy_document" "caller_read_only" {
  statement {
    sid     = "SearchMemoriesOnly"
    actions = ["execute-api:Invoke"]
    resources = [
      "${aws_apigatewayv2_api.memory.execution_arn}/*/POST/memories/search",
      "${aws_apigatewayv2_api.memory.execution_arn}/*/GET/memories/*",
    ]
  }
}

resource "aws_iam_policy" "caller_read_write" {
  name        = "${var.name_prefix}-caller"
  description = "Store, search and delete memories through the API."
  policy      = data.aws_iam_policy_document.caller_read_write.json
}

resource "aws_iam_policy" "caller_read_only" {
  name        = "${var.name_prefix}-caller-read-only"
  description = "Search and read memories, but never create or delete them."
  policy      = data.aws_iam_policy_document.caller_read_only.json
}
