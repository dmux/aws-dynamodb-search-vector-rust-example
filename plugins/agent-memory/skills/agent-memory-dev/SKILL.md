---
name: agent-memory-dev
description: Contributing to the agent-memory Rust workspace — the hexagonal dependency rule, which crate a change belongs in, how to add or change an MCP tool, and the non-obvious invariants that have tests guarding them. Use when editing any crate in this repo, adding a memory operation or MCP tool, or reviewing a change to it.
---

# Working on the agent-memory codebase

## The dependency rule is compiler-enforced

The workspace is split so the **build fails** on an architecture violation
rather than the boundary eroding quietly.

| Crate | Role | May depend on |
|---|---|---|
| `agent-memory-core` | Domain: model, ports, use cases | **nothing** |
| `agent-memory-contract` | Wire DTOs shared by server and client | serde |
| `agent-memory-aws` | Driven adapters: DynamoDB, Bedrock | core |
| `agent-memory-client` | `MemoryService` over HTTP + SigV4 | core, contract |
| `agent-memory-lambda` | Driving adapter: the HTTP API | core, contract, aws |
| `agent-memory-mcp` | Driving adapter: MCP over stdio | core, client |

**`agent-memory-core` has no AWS SDK dependency, and must never acquire one.**
Adding `aws-sdk-*` to its `Cargo.toml` is the one change that is always wrong —
it is what makes the tools testable against an in-memory fake with no network
and no AWS account. If domain code seems to need an AWS type, the answer is a
new method on a driven port, not a dependency.

`MemoryService` is the **primary port**. The Lambda and the MCP server are two
driving adapters over it, and neither knows the other exists.

## Where a change belongs

Ask what kind of thing you are changing:

- **A rule about what a memory is** — kinds, validation, TTL semantics, ranking
  → `core` (`model.rs` for types, `service.rs` for orchestration).
- **A new capability the domain needs from outside** → a method on a driven port
  in `core/ports.rs`, then implement it in `aws`.
- **How something is stored or embedded** → `agent-memory-aws` only. DynamoDB
  attribute names, Titan request shapes and `Score`→`Distance` conversion live
  there and nowhere else.
- **The HTTP wire shape** → `contract`, and both sides that use it.
- **What the model sees or can call** → `agent-memory-mcp/src/tools.rs`.

A change that touches `core` and an adapter in the same commit is normal. A
change that touches only `core` but mentions DynamoDB is a design error.

## Invariants with tests behind them

Break one of these and a test fails. The tests exist because each mistake is
silent and expensive to diagnose.

**No tool takes a `user_id`.** Identity comes from the signed request. An
argument the model can set is an argument the model can get wrong — a privilege
escalation. `no_tool_exposes_a_user_id_argument` serialises the schemas and
asserts the string is absent.

**Every tool description says when *not* to call it.** Descriptions are prompts,
not documentation: the model reads them to decide whether to call. Without a
hedge an agent recalls every turn, and each recall costs an embedding plus a
billed vector search. `every_tool_description_says_when_not_to_call_it` fails if
a description stops containing "Do not" or "Use it when". It also asserts the
tool count — update it when adding a tool.

**`Distance` is lower-is-better.** A `COSINE` index returns 0 for identical
direction, which is the opposite of what "score" implies. The direction is
fixed once in `Ord`, so no use case can invert it, and only the DynamoDB
adapter knows about AWS's `Score`. Never introduce a "score" into the domain.

**`TopK` is bounded 1..=100** — a hard `SearchVectors` quota, validated at the
edge so an out-of-range request fails *before* anyone pays for an embedding.

**`Memory` has no embedding field.** The vector is a storage detail derived from
`text`, and travels beside the memory only on `MemoryRepository::save`. Adding
it as a field would force every read path to carry a 1024-float array it
discards, or an `Option` that is always `None`.

**Bad input maps to `invalid_params`, everything else to `internal_error`.** The
model can act on the first by retrying; the second just looks like a broken
server.

## Adding an MCP tool

1. Args and result structs with `schemars::JsonSchema`, doc comments on every
   field — the model reads those as the parameter descriptions.
2. The method on `MemoryTools` inside the `#[tool_router]` impl, with a
   `description` that says both when to call **and when not to**.
3. Validate bounds before the service call, so nothing out of range reaches the
   network or a paid embedding.
4. Map errors through `to_error_data`.
5. Update the tool-count assertion in
   `every_tool_description_says_when_not_to_call_it`.
6. Tests against the in-memory fake — `LocalMemoryService` over
   `InMemoryRepository`, `StubEmbedder`, `FixedClock`, `SeqIdGenerator`. No
   network, no AWS.
7. If it needs a new server-side operation, add the route in
   `agent-memory-lambda/src/router.rs` and the DTOs in `contract`.
8. Update `docs/mcp.md`, and the `memory-curation` / `memory-recall` skills in
   this plugin if the tool changes how memory should be used.

Note that a new tool is also a new prompt in every session's context. Prefer a
new argument on an existing tool where the semantics allow it.

## Gotchas that have already cost time

**stdout belongs to JSON-RPC.** A stray `println!` in the MCP server corrupts
the protocol stream and the failure looks nothing like its cause. The tracing
subscriber is pinned to stderr explicitly, not by default. Never print to
stdout from `agent-memory-mcp`.

**`Implementation::from_build_env()` is the wrong helper.** It expands inside
the rmcp crate, so the server reports the SDK's name and version and shows up
as "rmcp 3.1.2" in every client's server list. Use `env!("CARGO_PKG_NAME")`.

**API Gateway payload format 1.0 is load-bearing.** Format 2.0's
`requestContext` has no `identity` object, so an `AWS_IAM` route gives the
Lambda no way to learn who called. Switching it breaks identity entirely and
surfaces as `401 unidentified_caller`.

**Always re-embed from the text being stored.** DynamoDB never recomputes
embeddings, so any write path that stores text without a matching vector
produces an item that is unfindable by search but present in the table.

**`#[tool_router]` cannot be applied to a generic impl**, which is why
`MemoryTools` holds `Arc<dyn MemoryService>` rather than a type parameter. The
dynamic dispatch is immaterial next to the network round trip it wraps.

## Before committing

```bash
make check     # fmt --check, clippy -D warnings, all unit tests. Free.
```

Integration tests (`make test-it`) hit real AWS, create resources and cost
money. They are `#[ignore]`d and gated on `DDB_VECTOR_IT=1` — do not run them
casually, and never in a loop.
