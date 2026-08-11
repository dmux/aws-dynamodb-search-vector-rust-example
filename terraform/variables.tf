variable "region" {
  description = "AWS region. Must offer both DynamoDB vector search and the Bedrock embedding model."
  type        = string
  default     = "us-east-1"
}

variable "name_prefix" {
  description = "Prefix applied to every resource name."
  type        = string
  default     = "agent-memory"
}

variable "vector_index_name" {
  description = "Name of the DynamoDB vector index."
  type        = string
  default     = "MemoryIndex"
}

variable "embedding_model_id" {
  description = "Bedrock embedding model. Must be enabled in this account and region."
  type        = string
  default     = "amazon.titan-embed-text-v2:0"
}

variable "embedding_dimensions" {
  description = <<-EOT
    Vector dimension. Must match the embedding model's output and cannot be
    changed after the index is created. Titan Text Embeddings V2 supports 256,
    512 and 1024.
  EOT
  type        = number
  default     = 1024

  validation {
    condition     = contains([256, 512, 1024], var.embedding_dimensions)
    error_message = "Titan Text Embeddings V2 only produces 256, 512 or 1024 dimensions."
  }
}

variable "lambda_artifact" {
  description = "Path to the zip produced by `make build`."
  type        = string
  default     = "../target/lambda/agent-memory-lambda/bootstrap.zip"
}

variable "lambda_memory_mb" {
  description = <<-EOT
    Lambda memory. CPU scales with this setting, so it also governs cold start
    and how fast a 1024-float response is parsed.
  EOT
  type        = number
  default     = 128
}

variable "lambda_timeout_seconds" {
  description = "Lambda timeout. A Bedrock InvokeModel call sits on the request path."
  type        = number
  default     = 30
}

variable "log_retention_days" {
  description = "CloudWatch Logs retention. Set explicitly so logs do not accumulate forever."
  type        = number
  default     = 14
}

variable "enable_access_logs" {
  description = <<-EOT
    Enable API Gateway access logs. Off by default because CloudWatch Logs
    charges for ingestion, and the API Gateway request itself is the cheapest
    part of this system.
  EOT
  type        = bool
  default     = false
}
