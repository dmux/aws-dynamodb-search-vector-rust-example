---
description: Inventory the memory store and report duplicates, contradictions, stale entries and leaked secrets
allowed-tools: ["Agent", "Skill", "Bash", "mcp__agent-memory__memory_recall", "mcp__agent-memory__memory_forget"]
argument-hint: "[topic to scope the audit to]"
---

# Audit the memory store

Scope: $ARGUMENTS — if empty, audit everything.

Launch the **memory-auditor** agent. It sweeps the store with probe queries,
cross-checks the local write journal at `~/.claude/agent-memory/journal.jsonl`,
and classifies what it finds.

A full sweep is the most expensive operation this system offers — an embedding
plus a per-byte-billed vector search per probe, each returning up to 100 items.
For a single suspect memory, that is the wrong tool: one targeted
`memory_recall` answers it.

If a scope was given, tell the auditor to probe only that topic area, which
makes the sweep much cheaper and its coverage claim correspondingly narrower.

## After the report

Relay the findings, secrets first if any. Then ask what to act on.

**Delete nothing without explicit approval.** `memory_forget` is irreversible —
no undo, and the text is unrecoverable. Approve per group, or per item where
anything is ambiguous.

For contradictions, ask the user which version holds; do not resolve by
recency. For leaked secrets, say clearly that deletion does not undo the
exposure and the credential must be rotated.

Be honest about coverage: there is no list endpoint, so no sweep is provably
complete. Report how many probes ran and how many distinct memories they
reached.
