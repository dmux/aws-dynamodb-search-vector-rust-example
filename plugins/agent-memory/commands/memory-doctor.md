---
description: Diagnose the whole agent-memory path — binary, config, credentials, deployment, and a live round trip
allowed-tools: ["Bash", "Read", "Skill", "mcp__agent-memory__memory_recall"]
---

# Memory doctor

Diagnose the agent-memory setup end to end and report the **first** thing that
is actually broken. Load the `agent-memory-ops` skill for what each symptom
means.

Work down the chain in order and stop at the first failure — each step depends
on the ones above it, so reporting later failures is noise.

**1. The MCP binary exists**

```bash
ls -l target/release/agent-memory-mcp 2>/dev/null || echo "MISSING"
```

Missing → `make build-mcp`.

**2. The client config points at it**

```bash
cat .mcp.json 2>/dev/null || echo "no .mcp.json"
```

Check the `command` path exists and that `env` carries both `AGENT_MEMORY_API`
and `AWS_REGION`. Missing env is the most common single cause of failure.

**3. Credentials resolve**

```bash
aws sts get-caller-identity
```

The ARN it prints **is** the memory namespace. If the user says memories
vanished, compare this against the profile that wrote them — a different
principal is a different partition, and this is nearly always the answer.

**4. The stack is deployed and matches the config**

```bash
cd terraform && terraform output 2>/dev/null || echo "not deployed"
```

Compare `api_endpoint` to `AGENT_MEMORY_API` in `.mcp.json`. A stale endpoint
after a redeploy is common and produces confusing failures.

**5. The tools are live**

Call `memory_recall` with query `"connectivity check"`, `top_k: 1`. This is one
embedding plus one small vector search — a fraction of a cent, and it proves
signing, IAM, the Lambda, Bedrock and the vector index all work together.

- Tool not available at all → the MCP server did not start. Run the binary
  directly with `RUST_LOG=debug` and read stderr.
- `403` → the caller lacks `execute-api:Invoke`; attach `caller_policy_arn`.
- `401 unidentified_caller` → the API Gateway integration is on payload format
  2.0 and must be 1.0.
- Empty result, no error → **healthy**. An empty store is not a failure.

**6. The journal**

```bash
wc -l ~/.claude/agent-memory/journal.jsonl 2>/dev/null || echo "no journal yet"
```

Absent is fine if nothing has been stored since the plugin was installed. If
memories have been written since and it is still absent, the PostToolUse hook
is not firing — check the plugin is enabled and `python3` is on PATH.

## Report

One line per step: `ok` or the failure and its fix. End with a verdict, and if
something is broken, the single next command to run.

Do not run `make deploy`, `make destroy`, `make smoke` or `make test-it` — they
cost money or change infrastructure. Diagnose, then recommend.
