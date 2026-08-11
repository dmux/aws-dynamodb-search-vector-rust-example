---
description: Build the MCP server and wire .mcp.json to the deployed API
allowed-tools: ["Bash", "Read", "Edit", "Write", "AskUserQuestion", "Skill"]
---

# Wire up the memory MCP server

Build the local MCP binary and point the client at the deployed API. Load the
`agent-memory-ops` skill if anything fails.

**1. Check the stack is deployed**

```bash
cd terraform && terraform output -raw api_endpoint 2>/dev/null || echo "not deployed"
```

If it is not deployed, stop and tell the user: deploying costs money and
requires Bedrock model access for `amazon.titan-embed-text-v2:0` in the target
region. Offer `make deploy`, but do not run it without approval. `make
preflight` is free and worth running first — `zig` is a hard requirement for
the cross-compile.

**2. Generate the config**

```bash
make mcp-config
```

This builds the binary and prints the `.mcp.json` block filled in from the
Terraform outputs.

**3. Write it**

If `.mcp.json` does not exist, create it with that block. If it exists with an
`agent-memory` entry, compare — if the `command` path or `AGENT_MEMORY_API`
differ, show the user the diff and update on confirmation. A stale endpoint
after a redeploy is a common and confusing failure.

If it exists with *other* servers, merge the `agent-memory` key in; never
overwrite the file wholesale.

**4. Enable and verify**

The server is enabled per-project in `.claude/settings.local.json`:

```json
{ "enabledMcpjsonServers": ["agent-memory"] }
```

Tell the user to **restart the client** — a new `.mcp.json` is read at startup.
Then suggest `/memory-doctor` to confirm the whole path, or `make smoke` (costs
a fraction of a cent) to prove it independently of the client.

No secret is involved anywhere in this: the server signs with whatever AWS
credentials the machine already has.
