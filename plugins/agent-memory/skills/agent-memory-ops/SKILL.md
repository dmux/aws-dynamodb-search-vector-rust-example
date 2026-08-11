---
name: agent-memory-ops
description: Deploy, wire and troubleshoot the agent-memory stack — Terraform, the Lambda build, the DynamoDB vector index, IAM for callers, the MCP client wiring, and what each failure symptom actually means. Use when deploying or destroying the stack, when memory tools fail with 403/401/credential errors, when the MCP server will not start, or when investigating cost.
---

# Operating the agent-memory stack

The deployed system is: API Gateway HTTP API (`AWS_IAM` auth) → Lambda (arm64,
Rust) → DynamoDB table with a vector index, plus Bedrock for embeddings. The
MCP server runs locally and signs requests with SigV4.

## The Make targets

Everything goes through the Makefile. Prefer these over raw `cargo`/`terraform`.

| Target | Does | Costs money |
|---|---|---|
| `make preflight` | Checks `cargo-lambda` and `zig` | no |
| `make check` | `fmt --check`, `clippy -D warnings`, all unit tests | no |
| `make build` | Lambda zip, arm64 | no |
| `make build-mcp` | Local MCP binary at `target/release/agent-memory-mcp` | no |
| `make plan` | `terraform plan` | no |
| `make deploy` | `build` then `terraform apply` | **yes** |
| `make smoke` | Store + recall one memory through the live API | **yes** (cents) |
| `make mcp-config` | Prints the `.mcp.json` block from Terraform outputs | no |
| `make test-it` | Integration tests against real AWS; creates resources | **yes** |
| `make destroy` | `terraform destroy` | — |

`make check` is the gate before any commit: it is free, needs no AWS account,
and covers the whole workspace.

## First deployment, in order

1. **Bedrock model access.** `amazon.titan-embed-text-v2:0` must be enabled in
   the target region. This is a console action per account and region; nothing
   in Terraform can grant it, and the failure surfaces much later as an
   `AccessDeniedException` on `InvokeModel`.
2. `make preflight` — `zig` is not optional. `cargo-lambda` cross-compiles
   through `cargo-zigbuild`; a natively linked binary picks up the build
   machine's glibc, which is newer than Amazon Linux 2023's, and the function
   dies at runtime with a GLIBC version error. Install with
   `pip3 install ziglang`.
3. `make deploy`.
4. `make smoke` — proves signing, IAM, the Lambda, Bedrock and the vector index
   all work together. If this passes, the MCP server will work.
5. `make mcp-config` and paste the output into `.mcp.json`, then restart the
   client.

## Key configuration

Terraform variables worth knowing (`terraform/variables.tf`):

| Variable | Default | Note |
|---|---|---|
| `region` | `us-east-1` | Must be a region where the Titan model is enabled |
| `embedding_dimensions` | `1024` | 256, 512 or 1024 — **cannot be changed in place** |
| `embedding_model_id` | `amazon.titan-embed-text-v2:0` | Changing it invalidates every stored vector |
| `vector_index_name` | `MemoryIndex` | |
| `lambda_memory_mb` | `128` | Also buys CPU, which sets JSON parse speed for a 1024-float response |
| `enable_access_logs` | `false` | CloudWatch Logs cost more than the API Gateway requests |

**Dimensions and model are effectively immutable.** A vector index cannot be
resized, and vectors from a different model are not comparable to existing
ones — rankings become meaningless rather than merely worse. Changing either
means a new index and a re-embed of every memory. Decide before the first
write.

Useful outputs: `api_endpoint`, `region`, `table_name`, `vector_index_arn`,
`lambda_function_name`, `caller_policy_arn`, `caller_read_only_policy_arn`.

## Granting a caller access

Identity is the caller's IAM principal — the ARN that signed the request,
normalised server-side into the DynamoDB partition key. There are no API keys
and no user records.

- Read-write: attach `caller_policy_arn`.
- Search-only (an agent that may read but must not write or delete): attach
  `caller_read_only_policy_arn`.

Two consequences worth internalising:

- **Different principal, different memories.** Switching AWS profiles switches
  the memory namespace. A user who "lost their memories" has almost always
  changed profile, role, or account.
- Per-user isolation is the table's partition key, which is also the vector
  index `HASH`. A search physically cannot span users.

## Troubleshooting

Work down this table; it is ordered by how often each cause is the real one.

| Symptom | Cause | Fix |
|---|---|---|
| `AGENT_MEMORY_API is not set` | `env` block missing from `.mcp.json` | `make mcp-config`, paste, restart client |
| `no AWS credentials available` | Client's environment has no profile | Credentials must exist for the process the MCP client spawns, which does not inherit your shell |
| `403 Forbidden` | Caller lacks `execute-api:Invoke` | Attach `caller_policy_arn` to the principal |
| `401 unidentified_caller` | Integration on payload format **2.0** | Must be 1.0 — 2.0's `requestContext` has no `identity`, so the Lambda cannot learn who called |
| Recall empty right after remember | Eventual consistency | Wait a few seconds. Not a failure |
| Recall empty, everything else fine | Different IAM principal than the writes | Check `aws sts get-caller-identity` against what wrote them |
| Garbled protocol errors in the client | Something wrote to **stdout** | stdout belongs to JSON-RPC; all diagnostics go to stderr |
| `AccessDeniedException` on `InvokeModel` | Bedrock model not enabled in region | Enable Titan V2 in the console for that account and region |
| GLIBC version error at runtime | Built without zigbuild | `make preflight`, install `zig`, rebuild |
| Vector index missing after apply | Provider gap on index creation | `terraform/scripts/vector-index.sh` is the escape hatch; see `docs/deployment.md` |

Diagnostics:

```bash
aws sts get-caller-identity                       # which principal am I?
aws logs tail "$(cd terraform && terraform output -raw lambda_function_name)" --follow
RUST_LOG=debug ./target/release/agent-memory-mcp  # MCP detail, safely on stderr
```

To verify the MCP server standalone, without a client, drive a JSON-RPC
handshake over stdin — the exact invocation is in `docs/mcp.md`.

## Where the money goes

- **Vector search is billed per byte processed**, which makes per-user
  partitioning the biggest cost lever in the system: a search reads one
  person's vectors, not the table's.
- `top_k: 100` sweeps are the most expensive operation available. See the
  `memory-audit` skill.
- Bedrock embeddings are charged per input token, on both write and recall.
- HTTP APIs were chosen over REST APIs for roughly a 3.5× lower request price.
- Access logs are off by default because CloudWatch Logs would exceed the API
  Gateway cost at this volume.

`docs/cost.md` has the verified numbers.

## Destroying

`make destroy` removes the table, and with it every stored memory. There is no
backup and no export path. Run a `memory-audit` sweep first if anything in
there matters.
