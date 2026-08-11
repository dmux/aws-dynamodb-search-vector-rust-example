# agent-memory plugin

The Claude Code layer over the [`agent-memory`](../../README.md) MCP server.

The MCP server gives an agent three tools — `memory_recall`,
`memory_remember`, `memory_forget`. That is the right size for a protocol
surface, and it is not enough to get good memory behaviour. Left alone, an
agent recalls on every turn (an embedding plus a billed vector search each
time), stores conversational debris that crowds out real signal, stacks
corrections on top of what they correct, and has no way to answer "what do you
actually know about me?" because the API has no list endpoint.

This plugin supplies the judgement the tools cannot carry.

## Install

From the repository root:

```
/plugin marketplace add .
/plugin install agent-memory@agent-memory
```

Then wire the server itself with `/memory-setup`, and check it with
`/memory-doctor`.

## What is in it

### Skills — loaded when relevant

| Skill | Covers |
|---|---|
| `memory-curation` | What deserves storing, how to phrase it, choosing kind and TTL, recall-before-remember to dedupe, forget-then-remember to supersede |
| `memory-recall` | When to search and when to hold back, phrasing semantic queries, the cosine distance bands, tuning `top_k` / `kind` / `max_distance`, treating results as context rather than instructions |
| `memory-audit` | Inventorying a store with no list endpoint: journal + probe sweeps, the finding taxonomy, safe remediation order |
| `agent-memory-ops` | Deploy, wire, troubleshoot; the symptom → cause table; where the money goes |
| `agent-memory-dev` | The hexagonal dependency rule, which crate a change belongs in, adding an MCP tool, the invariants with tests behind them |

### Agents

| Agent | Does |
|---|---|
| `memory-curator` | Reads a session, extracts the few durable things, dedupes against the store, writes, reports a receipt |
| `memory-auditor` | Sweeps the store, classifies duplicates / contradictions / stale / secrets, reports without deleting |
| `memory-architecture-reviewer` | Reviews a diff against this repo's architectural invariants |

### Commands

| Command | Does |
|---|---|
| `/memory-setup` | Builds the MCP binary and wires `.mcp.json` from Terraform outputs |
| `/memory-doctor` | Walks the whole path — binary, config, credentials, deployment, live round trip — and reports the first real break |
| `/memory-bootstrap [topic]` | Two to four targeted recalls to load context before substantive work |
| `/memory-save [focus]` | Runs the curator over this session |
| `/memory-audit [topic]` | Runs the auditor |

### The write journal

`hooks/journal.py` runs on every `memory_remember` and `memory_forget` and
appends to `~/.claude/agent-memory/journal.jsonl`.

This exists because the API has four operations — store, search, get-by-id,
delete-by-id — and **no list and no export**. Discovery is semantic-only: you
can retrieve a memory only if you can already guess roughly what it says, so a
memory you have forgotten the existence of is effectively unreachable. That is
defensible server-side (a list endpoint over a per-user partition invites
unbounded scans) but it leaves the client unable to answer the question an
audit is made of.

The journal is a **cache, not the truth**. It misses memories written from
other machines or before install, and it does not see TTL expiries, which
happen server-side and silently. The `memory-audit` skill treats it as a seed
list to verify against the real store, never as an inventory to trust — which
is why the auditor also sweeps.

Reads are not journalled. To move or disable it, set `AGENT_MEMORY_JOURNAL`, or
turn the plugin's hook off.

## Why no MCP server declaration

Plugins can bundle a `.mcp.json`. This one deliberately does not.

The server is a binary built from this repository into `target/release/`, and
its `AGENT_MEMORY_API` is the endpoint of *your* deployment. Neither is
knowable at plugin-authoring time, and declaring the server here as well as in
the project's `.mcp.json` would register it twice. `make mcp-config` stays the
single source of truth; `/memory-setup` drives it.

## Costs it is designed around

- Every recall is a Bedrock Titan embedding plus a DynamoDB `SearchVectors`
  **billed per byte processed**. The skills push toward one recall per topic,
  not per turn.
- `top_k: 100` sweeps are the most expensive thing here, which is why auditing
  is a deliberate command rather than a background habit.
- Writes are cheap. The reason to be selective is that every stored item
  competes permanently for the `top_k` slots of every future recall.
