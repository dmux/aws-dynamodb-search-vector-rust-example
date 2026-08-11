#!/usr/bin/env python3
"""Append every memory write to a local journal.

Why this exists
---------------
The memory API has four operations: POST /memories, POST /memories/search,
GET /memories/{id} and DELETE /memories/{id}. There is no list and no export.
Discovery is semantic-only: you can find a memory if you can already guess
roughly what it says, and a memory you have forgotten the existence of is
effectively unreachable.

That is a deliberate and defensible server-side design -- a list endpoint over
a per-user partition invites unbounded scans, and the store is meant to be
searched by meaning. But it leaves the *client* with no way to answer "what do
you actually know about me?", which is exactly the question an audit needs.

This hook closes that gap locally. It records the id, kind and text of every
memory written from this machine, and tombstones the ones deleted from it. The
journal is a cache, not the source of truth: memories written from another
machine are absent, and TTL expiries happen server-side without notice. The
memory-audit skill treats it accordingly -- as a seed list to verify against
the real store, never as an inventory to trust.

Reads are not journalled. Only writes.
"""

import json
import os
import sys
from datetime import datetime, timezone

JOURNAL = os.path.expanduser(
    os.environ.get("AGENT_MEMORY_JOURNAL", "~/.claude/agent-memory/journal.jsonl")
)

REMEMBER = "memory_remember"
FORGET = "memory_forget"


def unwrap(response):
    """Pull the tool's JSON payload out of whatever envelope it arrived in.

    MCP results reach a hook as structured content, as a list of content
    blocks with a JSON string inside, or already decoded, depending on the
    tool and the client version. Try each shape rather than assuming one.
    """
    if isinstance(response, dict):
        for key in ("structuredContent", "structured_content"):
            if isinstance(response.get(key), dict):
                return response[key]
        content = response.get("content")
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get("type") == "text":
                    try:
                        parsed = json.loads(block.get("text", ""))
                    except (ValueError, TypeError):
                        continue
                    if isinstance(parsed, dict):
                        return parsed
        return response
    if isinstance(response, str):
        try:
            parsed = json.loads(response)
        except ValueError:
            return {}
        return parsed if isinstance(parsed, dict) else {}
    return {}


def entry_for(tool_name, tool_input, result):
    """Build the journal line, or None if this write should not be recorded."""
    now = datetime.now(timezone.utc).isoformat()

    if tool_name.endswith(REMEMBER):
        memory_id = result.get("memory_id")
        if not memory_id:
            # The call failed or returned an unexpected shape. A journal entry
            # without an id is worse than no entry: it cannot be verified and
            # cannot be forgotten.
            return None
        return {
            "event": "remember",
            "at": now,
            "memory_id": memory_id,
            "kind": result.get("kind") or tool_input.get("kind") or "fact",
            "text": tool_input.get("text", ""),
            "ttl_seconds": tool_input.get("ttl_seconds"),
            "cwd": os.environ.get("CLAUDE_PROJECT_DIR", ""),
        }

    if tool_name.endswith(FORGET):
        if not result.get("forgotten"):
            return None
        memory_id = tool_input.get("memory_id")
        if not memory_id:
            return None
        return {"event": "forget", "at": now, "memory_id": memory_id}

    return None


def main():
    # A hook that fails must never break the tool call it observed, so every
    # unexpected input shape exits 0 and silently records nothing.
    try:
        event = json.load(sys.stdin)
    except (ValueError, OSError):
        return 0

    tool_name = event.get("tool_name") or ""
    if not (tool_name.endswith(REMEMBER) or tool_name.endswith(FORGET)):
        return 0

    tool_input = event.get("tool_input") or {}
    result = unwrap(event.get("tool_response"))
    if not isinstance(tool_input, dict):
        return 0

    entry = entry_for(tool_name, tool_input, result)
    if entry is None:
        return 0

    try:
        os.makedirs(os.path.dirname(JOURNAL), exist_ok=True)
        with open(JOURNAL, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry, ensure_ascii=False) + "\n")
    except OSError:
        return 0

    return 0


if __name__ == "__main__":
    sys.exit(main())
