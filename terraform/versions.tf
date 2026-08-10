terraform {
  required_version = ">= 1.9"

  required_providers {
    aws = {
      source = "hashicorp/aws"
      # No version of this provider supports DynamoDB vector indexes yet, which
      # is why dynamodb.tf carries an escape hatch. See the comment there.
      version = "~> 6.0"
    }
  }
}

provider "aws" {
  region = var.region

  default_tags {
    tags = {
      Project   = "agent-memory"
      ManagedBy = "terraform"
    }
  }
}

data "aws_caller_identity" "current" {}
