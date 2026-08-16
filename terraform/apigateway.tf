# An HTTP API, not a REST API.
#
# HTTP APIs cost $1.00 per million requests against $3.50 for REST, and they
# support everything this design uses (IAM authorization, Lambda proxy
# integration) while omitting what it does not (API keys, per-client throttling,
# caching, WAF, X-Ray). The omission that matters: HTTP APIs have no API keys at
# all, which is what makes IAM the natural choice — and IAM costs nothing and
# needs no secret distributed to clients.
resource "aws_apigatewayv2_api" "memory" {
  name          = var.name_prefix
  description   = "Semantic memory layer for AI agents."
  protocol_type = "HTTP"
}

resource "aws_apigatewayv2_integration" "memory" {
  api_id           = aws_apigatewayv2_api.memory.id
  integration_type = "AWS_PROXY"
  integration_uri  = aws_lambda_function.memory.invoke_arn

  # Pinned to 1.0 deliberately, and this is load-bearing.
  #
  # Payload format 2.0 carries only `requestContext.authorizer` (with a `jwt`
  # object) and has no `identity` field whatsoever, so a function behind an
  # AWS_IAM route has no way to learn who called it. Format 1.0 carries
  # `requestContext.identity.userArn`, which is what the handler turns into the
  # memory namespace. Switching this to "2.0" makes every request fail closed
  # with 401 unidentified_caller.
  payload_format_version = "1.0"
}

locals {
  # Separate routes rather than one catch-all, so IAM can distinguish "may
  # search" from "may delete". See the caller policies in iam.tf.
  routes = {
    remember = "POST /memories"
    recall   = "POST /memories/search"
    # `GET /memories` and `GET /memories/{memory_id}` are two routes, not one
    # with an optional segment: HTTP APIs match the literal path, so the bare
    # collection is never reached by the item route.
    list   = "GET /memories"
    get    = "GET /memories/{memory_id}"
    update = "PUT /memories/{memory_id}"
    forget = "DELETE /memories/{memory_id}"
  }
}

resource "aws_apigatewayv2_route" "memory" {
  for_each = local.routes

  api_id    = aws_apigatewayv2_api.memory.id
  route_key = each.value
  target    = "integrations/${aws_apigatewayv2_integration.memory.id}"

  # SigV4. The caller signs with credentials it already has, and API Gateway
  # hands the verified principal to the function — so identity is never a value
  # the client asserts in a payload.
  authorization_type = "AWS_IAM"
}

resource "aws_cloudwatch_log_group" "api" {
  count = var.enable_access_logs ? 1 : 0

  name              = "/aws/apigateway/${var.name_prefix}"
  retention_in_days = var.log_retention_days
}

resource "aws_apigatewayv2_stage" "default" {
  api_id = aws_apigatewayv2_api.memory.id
  name   = "$default"
  # Automatic deployment is HTTP-API-only and costs nothing.
  auto_deploy = true

  dynamic "access_log_settings" {
    for_each = var.enable_access_logs ? [1] : []

    content {
      destination_arn = aws_cloudwatch_log_group.api[0].arn
      format = jsonencode({
        requestId      = "$context.requestId"
        routeKey       = "$context.routeKey"
        status         = "$context.status"
        responseLength = "$context.responseLength"
        latency        = "$context.responseLatency"
        # The verified caller, which is also what the function turns into a
        # memory namespace. Useful when reconciling who wrote what.
        callerArn = "$context.identity.userArn"
      })
    }
  }
}
