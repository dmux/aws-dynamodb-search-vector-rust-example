# Created explicitly rather than left to the Lambda service, which would create
# one with no expiry and outside Terraform's control.
resource "aws_cloudwatch_log_group" "lambda" {
  name              = "/aws/lambda/${var.name_prefix}"
  retention_in_days = var.log_retention_days
}

resource "aws_lambda_function" "memory" {
  function_name = var.name_prefix
  role          = aws_iam_role.lambda.arn

  filename         = var.lambda_artifact
  source_code_hash = filebase64sha256(var.lambda_artifact)

  # A Rust binary runs on the OS-only runtime, and arm64 (Graviton) is both
  # cheaper per GB-second and quicker to start here.
  runtime       = "provided.al2023"
  handler       = "bootstrap"
  architectures = ["arm64"]

  # CPU is allocated in proportion to memory, so this figure governs cold start
  # and how fast a 1024-float embedding is parsed, not just headroom.
  memory_size = var.lambda_memory_mb
  # A Bedrock InvokeModel call sits on the request path.
  timeout = var.lambda_timeout_seconds

  environment {
    variables = {
      TABLE_NAME           = aws_dynamodb_table.memories.name
      VECTOR_INDEX_NAME    = var.vector_index_name
      EMBEDDING_MODEL_ID   = var.embedding_model_id
      EMBEDDING_DIMENSIONS = tostring(var.embedding_dimensions)
      RUST_LOG             = "info"
    }
  }

  # Note the absence of a vpc_config. SearchVectors resolves to a dedicated
  # endpoint (search-dynamodb.<region>.amazonaws.com) separate from the regular
  # DynamoDB one. Inside a VPC that host needs its own egress path, and the
  # symptom of missing it is that writes succeed while only search fails.

  depends_on = [
    aws_cloudwatch_log_group.lambda,
    aws_iam_role_policy_attachment.lambda,
    # The function must not go live before the index can answer a search.
    terraform_data.vector_index,
  ]
}

resource "aws_lambda_permission" "api_gateway" {
  statement_id  = "AllowExecutionFromApiGateway"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.memory.function_name
  principal     = "apigateway.amazonaws.com"
  source_arn    = "${aws_apigatewayv2_api.memory.execution_arn}/*/*"
}
