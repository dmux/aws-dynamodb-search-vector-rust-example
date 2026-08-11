---
description: Curate this session into long-term memory — extract, dedupe, store
allowed-tools: ["Agent", "Skill", "mcp__agent-memory__memory_recall", "mcp__agent-memory__memory_remember", "mcp__agent-memory__memory_forget"]
argument-hint: "[what to focus on]"
---

# Save what matters from this session

Focus: $ARGUMENTS — if empty, review the whole conversation.

Delegate to the **memory-curator** agent via a `fork` subagent so it inherits
this conversation's context; it cannot curate what it cannot read. Instruct it
to follow its own definition and the `memory-curation` skill, and to focus on
the topic above if one was given.

If the user named something specific to remember, store that even if it looks
transient — they asked. You may note once, in one line, that it appears
short-lived and offer a TTL.

Relay the curator's report verbatim-ish: what was stored, what superseded what,
what was skipped and why. It is a receipt, so a wrong entry can be corrected on
the spot.

Expect a small number. Two or three good memories from a session is the normal,
healthy outcome; storing nothing is also a valid result and should be reported
as one line, not apologised for.
