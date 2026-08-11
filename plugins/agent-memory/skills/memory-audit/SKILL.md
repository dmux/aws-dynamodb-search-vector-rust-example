---
name: memory-audit
description: Inventory and clean a long-term memory store that has no list endpoint — sweep it with probe queries, cross-check the local write journal, and find duplicates, contradictions, stale entries and leaked secrets. Use when the user asks what is stored about them, wants memory cleaned up or exported, suspects a wrong or stale memory, or asks to be forgotten.
---

# Auditing the memory store

## The constraint that shapes everything here

The API has four operations — store, search, get-by-id, delete-by-id. **There
is no list and no export.** Discovery is semantic-only: you can retrieve a
memory only if you can already guess roughly what it says.

So a complete inventory is not obtainable. Say that plainly when you report,
and never present a sweep as exhaustive. What you can produce is good coverage
from two imperfect sources:

1. **The local write journal** — `~/.claude/agent-memory/journal.jsonl`, written
   by this plugin's PostToolUse hook. Exact ids and text for everything written
   *from this machine while the plugin was installed*.
2. **Probe sweeps** — broad `memory_recall` queries at high `top_k`, which find
   memories the journal never saw.

Neither alone is enough. The journal misses other machines and predates
nothing; sweeps miss whatever no probe came near. Use both, union the results.

## Step 1 — read the journal

```bash
test -f ~/.claude/agent-memory/journal.jsonl && wc -l < ~/.claude/agent-memory/journal.jsonl
```

Build the live set: every `remember` entry whose `memory_id` has no later
`forget` entry.

```bash
python3 - <<'PY'
import json, os, collections
path = os.path.expanduser("~/.claude/agent-memory/journal.jsonl")
live, forgotten = collections.OrderedDict(), set()
for line in open(path, encoding="utf-8"):
    try: e = json.loads(line)
    except ValueError: continue
    if e.get("event") == "remember": live[e["memory_id"]] = e
    elif e.get("event") == "forget": forgotten.add(e["memory_id"])
for mid in forgotten: live.pop(mid, None)
print(f"{len(live)} live / {len(forgotten)} forgotten")
for e in live.values():
    print(f"{e['memory_id']}\t{e['kind']}\t{e['at'][:10]}\t{e['text'][:90]}")
PY
```

The journal is a **cache, not truth**. An entry can be gone server-side — TTL
expiry deletes items silently, and a delete from another machine leaves no
trace here. Anything you plan to act on, verify with a targeted recall first.

If the journal does not exist, say so, note that only sweep coverage is
available, and continue at step 2.

## Step 2 — sweep with probes

Run broad probes at `top_k: 100`, one per kind, and union everything by
`memory_id`. Kind-filtered probes spend all 100 slots inside that kind, which
beats unfiltered probes at the same cost.

A workable probe set — adapt it to what you know of the user:

| Kind | Probes |
|---|---|
| `preference` | working style and communication preferences · tools, languages and frameworks preferred · code review and testing habits |
| `fact` | employer, role, team and projects · systems, infrastructure and accounts · personal details, location, schedule |
| `episode` | decisions made and their reasoning · problems encountered and how they were resolved |

Set `max_distance` to about `0.9` on probes — deliberately loose, because you
are enumerating rather than answering. Then ignore the distances when
deduplicating: for an audit, a distant hit is still a real stored memory, and
the distance only tells you the probe was a poor match for it.

Coverage is proportional to probe diversity, not to probe count. Eight probes
spread across unrelated topics beat twenty rephrasings of the same one.

Stop sweeping when a full round of probes returns no ids you have not already
seen. Report roughly how many distinct memories you found and how many probes
it took.

## Step 3 — classify what you found

Go through the union and sort every memory into one of these:

**Duplicates.** Two memories saying the same thing, usually written in
different sessions. Detect by pairwise closeness (≲ 0.15) or by reading. Keep
the better-phrased one — more self-contained, better kind, correct TTL — and
forget the other.

**Contradictions.** Two memories that cannot both be true. These are the most
damaging finding, because recall ranks by vector distance, which has no notion
of which is current. Present both to the user with their dates and ask which
holds. Never guess, even when one is obviously newer.

**Stale.** Anything with "currently", "this quarter", "migrating to", "working
on" that has aged past plausibility, and anything naming a file, flag or
command. Verify the named thing still exists — if it does not, the memory is
stale regardless of its wording.

**Over-broad or compound.** Several facts in one item. It matches everything
weakly and cannot be partially corrected. Propose splitting: forget one, store
two or three.

**Should never have been stored.** Secrets, transient state, anything
reconstructible from a repo. Secrets are urgent — flag them first and
separately, and note that deletion removes the item from DynamoDB but does not
undo whatever exposure already occurred. If a credential was stored, the fix is
to rotate it, not only to forget it.

**Healthy.** Say how many. A report that is all findings and no baseline reads
as alarmist.

## Step 4 — report before you delete

Show the findings first, grouped by class, most consequential first. Include
the `memory_id` for each so any item can be acted on individually.

**Never bulk-delete.** `memory_forget` is irreversible — there is no undo, no
soft delete, and no way to reconstruct the text once the item is gone. Deleting
one memory the user wanted is a worse outcome than leaving ten they did not.

Get explicit approval per group, or per item for anything ambiguous. The one
exception is a user asking to be forgotten wholesale: confirm scope once, then
proceed and report the count.

When consolidating, order matters — `memory_forget` the outdated ids **first**,
then `memory_remember` the replacement. Storing first risks leaving both if the
forget fails.

After writes, remember search is eventually consistent: a verification recall
run immediately may still show a forgotten memory. Wait a few seconds, or say
the change is applied and will settle shortly.

## What a sweep costs

Each probe is one Bedrock embedding plus one `SearchVectors` billed per byte
processed, and `top_k: 100` returns up to a hundred items of text. A sweep is
the most expensive thing this MCP server does — perhaps a few US cents at
typical store sizes, but tens of thousands of tokens of context.

That makes it a deliberate operation, not something to run speculatively. Do a
full sweep when the user asks for an audit or an export. For a single suspect
memory, one targeted recall is the right tool.
