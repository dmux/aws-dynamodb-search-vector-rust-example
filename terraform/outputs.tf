output "api_endpoint" {
  description = "Base URL of the memory API. Feed this to the MCP server as AGENT_MEMORY_API."
  value       = aws_apigatewayv2_stage.default.invoke_url
}

output "region" {
  description = "Region the stack was deployed to."
  value       = var.region
}

output "table_name" {
  description = "DynamoDB table holding the memories."
  value       = aws_dynamodb_table.memories.name
}

output "vector_index_arn" {
  description = "ARN of the vector index, the resource dynamodb:SearchVectors is granted on."
  value       = local.vector_index_arn
}

output "lambda_function_name" {
  description = "Lambda function name, for `aws logs tail`."
  value       = aws_lambda_function.memory.function_name
}

output "caller_policy_arn" {
  description = "Attach to a principal that should be able to store and search memories."
  value       = aws_iam_policy.caller_read_write.arn
}

output "caller_read_only_policy_arn" {
  description = "Attach to an agent that may search but must not write or delete."
  value       = aws_iam_policy.caller_read_only.arn
}
